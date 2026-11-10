//! Incremental byte streams owned by one Orb revision and scoped to one guest callback.
//!
//! Stream identities are issued by the host, are unique within the Orb and are never reused.
//! Each callback opens an epoch; every stream belongs to the epoch that connected it. When the
//! callback completes, is abandoned or its host closes, the epoch ends: its streams close at
//! once, pending reads and writes settle, and a connection still in flight is discarded instead
//! of becoming a usable handle.
//!
//! HTTP response bodies are entries of the same table: they share the identity sequence, the
//! per-Orb limit and the callback epochs, but live in their own map, so a socket identity can
//! never select a body or the reverse.
use super::http::Body;
use crate::{failure, protocol::TRANSPORT_CHUNK_BYTES};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    sync::{Arc, Mutex, MutexGuard},
    time::Instant,
};
use tembra::Result;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{
        TcpStream,
        tcp::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::{Mutex as Exclusive, OwnedMutexGuard, watch},
};

/// Live and connecting streams and HTTP bodies one Orb may hold at a time.
pub(super) const STREAM_LIMIT: usize = 16;

/// The transport table of one installed Orb revision.
#[derive(Clone)]
pub(crate) struct Streams(Arc<Shared>);

struct Shared {
    table: Mutex<Table>,
    /// The epoch of the active callback, or `None` between callbacks.
    scope: watch::Sender<Option<u64>>,
}

#[derive(Default)]
struct Table {
    /// Last issued stream identity.
    issued: u32,
    /// Last opened callback epoch.
    epoch: u64,
    active: Option<u64>,
    live: BTreeMap<u32, Arc<Stream>>,
    bodies: BTreeMap<u32, Arc<Body>>,
    /// Admitted connections and HTTP requests that have not yet registered or failed.
    connecting: usize,
}

/// One connected socket; the halves serialize reads and writes independently.
pub(super) struct Stream {
    epoch: u64,
    reader: Arc<Exclusive<OwnedReadHalf>>,
    writer: Arc<Exclusive<OwnedWriteHalf>>,
    closed: watch::Sender<bool>,
}

impl Default for Streams {
    fn default() -> Self {
        Self(Arc::new(Shared {
            table: Mutex::default(),
            scope: watch::Sender::new(None),
        }))
    }
}

impl Streams {
    fn table(&self) -> MutexGuard<'_, Table> {
        self.0
            .table
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// Open the epoch of a callback that has just been admitted on this Orb's Store.
    pub(crate) fn begin_callback(&self) {
        let epoch = {
            let mut table = self.table();
            table.epoch += 1;
            table.active = Some(table.epoch);
            table.epoch
        };
        self.0.scope.send_replace(Some(epoch));
    }

    /// End the active epoch and close every stream and body it owns. Idempotent.
    pub(crate) fn end_callback(&self) {
        let (closed, bodies) = {
            let mut table = self.table();
            table.active = None;
            (
                std::mem::take(&mut table.live),
                std::mem::take(&mut table.bodies),
            )
        };
        self.0.scope.send_replace(None);
        for stream in closed.into_values() {
            stream.close();
        }
        for body in bodies.into_values() {
            body.close();
        }
    }

    /// Check authority without changing the callback's existing epoch.
    pub(crate) fn active(&self) -> bool {
        self.table().active.is_some()
    }

    /// Capture this callback's epoch now and wake when its existing owner revokes it.
    pub(crate) fn cancelled(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
        let mut scope = self.0.scope.subscribe();
        let epoch = *scope.borrow();
        async move {
            if epoch.is_some() {
                let _ = scope.wait_for(|active| *active != epoch).await;
            }
        }
    }

    /// Admit one connection for the active callback within the per-Orb stream limit.
    pub(super) fn reserve(&self) -> Result<Reservation> {
        let mut table = self.table();
        let epoch = table
            .active
            .ok_or_else(|| failure("transport requires an active callback"))?;
        if table.live.len() + table.bodies.len() + table.connecting >= STREAM_LIMIT {
            return Err(failure("transport stream limit reached"));
        }
        table.connecting += 1;
        Ok(Reservation {
            streams: self.clone(),
            epoch,
        })
    }

    /// Resolve a live stream issued to the active callback of this Orb.
    pub(super) fn stream(&self, id: u32) -> Result<Arc<Stream>> {
        let table = self.table();
        let stream = table
            .live
            .get(&id)
            .ok_or_else(|| failure("unknown transport stream"))?;
        if table.active != Some(stream.epoch) {
            return Err(failure("unknown transport stream"));
        }
        Ok(stream.clone())
    }

    /// Resolve a live HTTP response body issued to the active callback of this Orb.
    pub(super) fn body(&self, id: u32) -> Result<Arc<Body>> {
        let table = self.table();
        let body = table
            .bodies
            .get(&id)
            .ok_or_else(|| failure("unknown HTTP response body"))?;
        if table.active != Some(body.epoch) {
            return Err(failure("unknown HTTP response body"));
        }
        Ok(body.clone())
    }

    /// Close an issued response body idempotently; pending reads settle immediately.
    pub(super) fn close_body(&self, id: u32) -> Result<Value> {
        let body = {
            let mut table = self.table();
            if id == 0 || id > table.issued || table.live.contains_key(&id) {
                return Err(failure("unknown HTTP response body"));
            }
            table.bodies.remove(&id)
        };
        if let Some(body) = body {
            body.close();
        }
        Ok(Value::Null)
    }

    /// Issue the next identity; identities are never reused within an Orb.
    fn issue(table: &mut Table) -> Result<u32> {
        let id = table
            .issued
            .checked_add(1)
            .ok_or_else(|| failure("transport stream identities are exhausted"))?;
        table.issued = id;
        Ok(id)
    }

    /// Close an issued stream idempotently, including cleanup after cancellation revoked it.
    pub(super) fn close(&self, id: u32) -> Result<Value> {
        let stream = {
            let mut table = self.table();
            if id == 0 || id > table.issued || table.bodies.contains_key(&id) {
                return Err(failure("unknown transport stream"));
            }
            table.live.remove(&id)
        };
        if let Some(stream) = stream {
            stream.close();
        }
        Ok(Value::Null)
    }

    /// Publish a connected socket only while its callback epoch is still active.
    fn register(&self, epoch: u64, socket: TcpStream) -> Result<u32> {
        let mut table = self.table();
        if table.active != Some(epoch) {
            return Err(failure(
                "callback ended before the transport stream connected",
            ));
        }
        let id = Self::issue(&mut table)?;
        let (reader, writer) = socket.into_split();
        table.live.insert(
            id,
            Arc::new(Stream {
                epoch,
                reader: Arc::new(Exclusive::new(reader)),
                writer: Arc::new(Exclusive::new(writer)),
                closed: watch::Sender::new(false),
            }),
        );
        Ok(id)
    }
}

/// An admitted connection slot; dropping it releases the slot whatever the outcome.
pub(super) struct Reservation {
    streams: Streams,
    epoch: u64,
}

impl Reservation {
    /// Connect within the operation deadline; the callback's end abandons the attempt.
    pub(super) async fn connect(self, address: SocketAddr, deadline: Instant) -> Result<Value> {
        let mut scope = self.streams.0.scope.subscribe();
        let epoch = self.epoch;
        let connect = tokio::time::timeout_at(deadline.into(), TcpStream::connect(address));
        let socket = tokio::select! {
            connected = connect => connected
                .map_err(|_| failure("host I/O deadline exceeded"))?
                .map_err(|error| failure(format!("transport connect: {error}")))?,
            _ = scope.wait_for(|active| *active != Some(epoch)) => {
                return Err(failure("callback ended before the transport stream connected"));
            }
        };
        self.streams.register(epoch, socket).map(Value::from)
    }
}

impl Reservation {
    /// Resolve once the callback that admitted this reservation has ended.
    pub(super) async fn abandoned(&self) {
        let epoch = self.epoch;
        let mut scope = self.streams.0.scope.subscribe();
        let _ = scope.wait_for(|active| *active != Some(epoch)).await;
    }

    /// Publish a response body only while its callback epoch is still active.
    pub(super) fn register_body(self, body: impl FnOnce(u64) -> Body) -> Result<u32> {
        let mut table = self.streams.table();
        if table.active != Some(self.epoch) {
            return Err(failure("callback ended before the HTTP response arrived"));
        }
        let id = Streams::issue(&mut table)?;
        table.bodies.insert(id, Arc::new(body(self.epoch)));
        Ok(id)
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.streams.table().connecting -= 1;
    }
}

impl Stream {
    fn close(&self) {
        self.closed.send_replace(true);
    }

    /// Claim the read half; a second concurrent read on the same stream is refused.
    pub(super) fn reader(&self) -> Result<OwnedMutexGuard<OwnedReadHalf>> {
        self.reader
            .clone()
            .try_lock_owned()
            .map_err(|_| failure("a transport read is already pending"))
    }

    /// Claim the write half; a second concurrent write on the same stream is refused.
    pub(super) fn writer(&self) -> Result<OwnedMutexGuard<OwnedWriteHalf>> {
        self.writer
            .clone()
            .try_lock_owned()
            .map_err(|_| failure("a transport write is already pending"))
    }

    /// Read one bounded chunk, `null` at end of stream.
    pub(super) async fn read(
        self: Arc<Self>,
        mut reader: OwnedMutexGuard<OwnedReadHalf>,
        deadline: Instant,
    ) -> Result<Value> {
        let mut closed = self.closed.subscribe();
        let mut buffer = vec![0; TRANSPORT_CHUNK_BYTES];
        let count = tokio::select! {
            read = tokio::time::timeout_at(deadline.into(), reader.read(&mut buffer)) => read
                .map_err(|_| failure("host I/O deadline exceeded"))?
                .map_err(|error| failure(format!("transport read: {error}")))?,
            _ = closed.wait_for(|closed| *closed) => return Err(failure("transport stream closed")),
        };
        if count == 0 {
            return Ok(Value::Null);
        }
        Ok(Value::Array(
            buffer[..count]
                .iter()
                .map(|byte| Value::from(*byte))
                .collect(),
        ))
    }

    /// Write one bounded chunk completely before replying.
    pub(super) async fn write(
        self: Arc<Self>,
        mut writer: OwnedMutexGuard<OwnedWriteHalf>,
        bytes: Vec<u8>,
        deadline: Instant,
    ) -> Result<Value> {
        let mut closed = self.closed.subscribe();
        tokio::select! {
            written = tokio::time::timeout_at(deadline.into(), writer.write_all(&bytes)) => written
                .map_err(|_| failure("host I/O deadline exceeded"))?
                .map_err(|error| failure(format!("transport write: {error}")))?,
            _ = closed.wait_for(|closed| *closed) => return Err(failure("transport stream closed")),
        }
        Ok(Value::Null)
    }
}
