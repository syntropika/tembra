//! Source-Orb I/O uses host-owned directory capabilities and exact destination grants.
//!
//! The guest supplies only typed requests. Filesystem access is standard WASI dispatched inside
//! the Orb's own Store (see `worker::standard`) under the shared active filesystem budget;
//! sockets are incremental, nonblocking streams on one shared reactor, so a slow reader never
//! occupies the embedding runtime's thread pool. HTTP runs the host's standard WASI HTTP client
//! on the same reactor; its response bodies are streams too.
use crate::{failure, protocol::AsyncRequest, protocol::TRANSPORT_CHUNK_BYTES};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};
use tembra::Result;

mod http;
mod reactor;
mod streams;

pub(crate) use streams::Streams;

/// Reserve one slot of the process-wide bounded filesystem admission.
pub(crate) fn admit_file() -> Result<tokio::sync::OwnedSemaphorePermit> {
    reactor::reactor()?.admit_file()
}

/// Bound inline upstream filesystem work without holding a worker across an async wait.
pub(crate) async fn file_turn<F: std::future::Future>(future: F) -> Result<F::Output> {
    Ok(reactor::reactor()?.file_turn(future).await)
}

/// Standard WASI context of one source installation, moved once into its Store.
pub(crate) struct StandardContext {
    pub wasi: wasmtime_wasi::WasiCtxBuilder,
    /// Checked contract imports; generated dispatch refuses any other interface.
    pub allowed: BTreeSet<String>,
    /// The existing callback epoch also revokes pending standard WASI calls.
    pub scope: Streams,
    /// Clocks for the interpreter's own WASIp1 `clock_time_get`, only with the clocks grant.
    pub clocks: Option<crate::worker::wasi::Clocks>,
}

/// Bound once for an installation; no directory handle or network authority enters the VM.
#[derive(Clone)]
pub(crate) struct SourceIo {
    destinations: Arc<BTreeSet<SocketAddr>>,
    /// HTTP origins are checked against these grants; raw socket grants never authorize HTTP.
    grants: Arc<crate::Grants>,
    streams: Streams,
    timeout: Duration,
    /// Taken by the Session that owns the Store; never shared between Stores.
    standard: Arc<std::sync::Mutex<Option<StandardContext>>>,
    /// Source built-in authority sent with Load, decided from system grants only.
    builtins: crate::protocol::Builtins,
}
impl SourceIo {
    pub(crate) fn new(
        providers: &crate::providers::Providers,
        imports: &[tembra_contracts::HostImport],
        grants: &crate::Grants,
        timeout: Duration,
        cli: &crate::Cli,
    ) -> Result<Self> {
        grants.check(
            imports,
            &providers.filesystem.keys().cloned().collect(),
            &providers.network,
        )?;
        let streams = Streams::default();
        let prepared = providers.context(imports, grants, cli)?;
        let standard = StandardContext {
            wasi: prepared.wasi,
            clocks: prepared.clocks,
            scope: streams.clone(),
            allowed: imports
                .iter()
                .map(|import| import.contract.clone())
                .collect(),
        };
        Ok(Self {
            destinations: Arc::new(
                grants
                    .network
                    .iter()
                    .flat_map(|grant| grant.connect.iter().copied())
                    .collect(),
            ),
            grants: Arc::new(grants.clone()),
            streams,
            timeout,
            standard: Arc::new(std::sync::Mutex::new(Some(standard))),
            builtins: prepared.builtins,
        })
    }

    /// Source built-in authority for this installation's Load.
    pub(crate) fn builtins(&self) -> crate::protocol::Builtins {
        self.builtins.clone()
    }

    /// Move the standard WASI context into the Store that owns this installation.
    pub(crate) fn take_standard(&self) -> Option<StandardContext> {
        self.standard.lock().expect("standard context").take()
    }

    /// The transport table whose epochs follow this Orb's guest callbacks.
    pub(crate) fn streams(&self) -> &Streams {
        &self.streams
    }

    /// Check one request against this installation and capture its deadline at admission.
    pub(crate) fn operation(&self, packet: &str) -> Result<IoOperation> {
        let request = match AsyncRequest::decode(packet).map_err(failure)? {
            AsyncRequest::TransportConnect { ip, port } => {
                let address = SocketAddr::new(
                    ip.parse()
                        .map_err(|_| failure("transport requires a literal IP address"))?,
                    port,
                );
                if !self.destinations.contains(&address) {
                    return Err(failure("transport destination is not granted to this Orb"));
                }
                Operation::Connect {
                    reservation: self.streams.reserve()?,
                    address,
                }
            }
            AsyncRequest::TransportRead { id } => {
                let stream = self.streams.stream(id)?;
                let reader = stream.reader()?;
                Operation::Read { stream, reader }
            }
            AsyncRequest::TransportWrite { id, bytes } => {
                if bytes.len() > TRANSPORT_CHUNK_BYTES {
                    return Err(failure("transport write exceeds the chunk limit"));
                }
                let stream = self.streams.stream(id)?;
                let writer = stream.writer()?;
                Operation::Write {
                    stream,
                    writer,
                    bytes,
                }
            }
            AsyncRequest::TransportClose { id } => Operation::Close {
                streams: self.streams.clone(),
                id,
            },
            AsyncRequest::HttpSend {
                url,
                method,
                headers,
                body,
            } => Operation::HttpSend {
                request: Box::new(http::Outgoing::new(
                    &self.grants,
                    &url,
                    &method,
                    headers,
                    body,
                )?),
                reservation: self.streams.reserve()?,
            },
            AsyncRequest::HttpRead { id } => {
                let (body, reader) = self.streams.http_body(id)?;
                Operation::HttpRead { body, reader }
            }
            AsyncRequest::HttpClose { id } => Operation::HttpClose {
                streams: self.streams.clone(),
                id,
            },
            _ => return Err(failure("expected an I/O operation")),
        };
        Ok(IoOperation {
            request,
            deadline: Instant::now() + self.timeout,
        })
    }
}

/// A Send operation owns its original installation's capabilities until physical completion.
enum Operation {
    Connect {
        reservation: streams::Reservation,
        address: SocketAddr,
    },
    Read {
        stream: Arc<streams::Stream>,
        reader: tokio::sync::OwnedMutexGuard<tokio::net::tcp::OwnedReadHalf>,
    },
    Write {
        stream: Arc<streams::Stream>,
        writer: tokio::sync::OwnedMutexGuard<tokio::net::tcp::OwnedWriteHalf>,
        bytes: Vec<u8>,
    },
    Close {
        streams: Streams,
        id: u32,
    },
    HttpSend {
        request: Box<http::Outgoing>,
        reservation: streams::Reservation,
    },
    HttpRead {
        body: Arc<http::Body>,
        reader: tokio::sync::OwnedMutexGuard<http::Reader>,
    },
    HttpClose {
        streams: Streams,
        id: u32,
    },
}

pub struct IoOperation {
    request: Operation,
    deadline: Instant,
}
impl IoOperation {
    /// Submit admitted host work without occupying the embedding runtime's worker pool.
    ///
    /// `complete` runs exactly once, on the submitting thread when admission fails and on a
    /// host I/O thread otherwise.
    pub fn submit(self, complete: impl FnOnce(Result<Value>) + Send + 'static) {
        let reactor = match reactor::reactor() {
            Ok(reactor) => reactor,
            Err(error) => return complete(Err(error)),
        };
        let deadline = self.deadline;
        match self.request {
            Operation::Connect {
                reservation,
                address,
            } => reactor.spawn(async move {
                complete(reservation.connect(address, deadline).await);
            }),
            Operation::Read { stream, reader } => reactor.spawn(async move {
                complete(stream.read(reader, deadline).await);
            }),
            Operation::Write {
                stream,
                writer,
                bytes,
            } => reactor.spawn(async move {
                complete(stream.write(writer, bytes, deadline).await);
            }),
            Operation::Close { streams, id } => complete(streams.close(id)),
            Operation::HttpSend {
                request,
                reservation,
            } => reactor.spawn(async move {
                complete(request.send(reservation, deadline).await);
            }),
            Operation::HttpRead { body, reader } => reactor.spawn(async move {
                complete(body.read(reader, deadline).await);
            }),
            Operation::HttpClose { streams, id } => complete(streams.close_body(id)),
        }
    }

    pub(crate) async fn execute(self) -> Result<Value> {
        let (send, receive) = futures::channel::oneshot::channel();
        self.submit(move |outcome| {
            let _ = send.send(outcome);
        });
        receive.await.map_err(failure)?
    }
}

#[cfg(test)]
mod http_tests;
#[cfg(test)]
mod tests;
