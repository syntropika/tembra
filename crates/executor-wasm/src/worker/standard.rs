//! Dispatch generated standard WASI calls inside the source interpreter's own Store.
//!
//! The binary packets come from bindings generated from the bundled WASI WIT. Native hosts call
//! the public upstream `Host`/`HostWithStore` traits here, between interpreter steps, so the
//! bytes never cross Node, UniFFI or another foreign carrier. Handles are issued by this table:
//! every borrow is checked for identity, liveness and resource type before upstream code sees a
//! `Resource`. Owned resources belong to the Store, which is this revision's lifetime.
use super::runtime::State;
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex, Weak},
};

pub(crate) mod adapter;
mod codec;
mod handles;
mod streams;

pub(crate) use adapter::{Compiled as StreamAdapter, attribute as attribute_trap};
use codec::{Reader, Writer};
use handles::{Close, Entry, Handles, Lease, Owned};
use streams::{
    ABANDONED_FUTURE, ItemConsumer, Pipe, Promise, Promised, ReadTurn, Slot, Writing, read_future,
    read_stream,
};

use tokio::sync::oneshot;
use wasmtime::{
    StoreContextMut,
    component::{JoinHandle, Resource},
};

include!(concat!(env!("OUT_DIR"), "/standard.rs"));

/// Leading byte of a binary standard packet; JSON packets never start with NUL.
pub(crate) const PACKET_TAG: u8 = crate::abi::STANDARD_PACKET as u8;
const CALL: u8 = 0;
const STREAM_READ: u8 = 1;
const FUTURE_READ: u8 = 2;
const DROP: u8 = 3;
const STREAM_NEW: u8 = 4;
const STREAM_WRITE: u8 = 5;
const FUTURE_NEW: u8 = 6;
const FUTURE_WRITE: u8 = 7;
/// Bytes buffered per stream before the upstream producer is paused.
const PIPE_BYTES: usize = 64 << 10;
/// Live handles per revision.
const HANDLE_LIMIT: usize = 4096;

/// One encoded standard result and the handles it publishes once the guest receives it.
pub(super) struct Delivery {
    pub(super) bytes: Vec<u8>,
    /// Revision-owned handles named only by `bytes`; rolled back if the reply is never delivered.
    pub(super) issued: Vec<u64>,
    /// The stream a STREAM_READ reply drained; poisoned if the reply is never delivered. Weak, so
    /// a queued reply never keeps a dropped stream's detached entry counted.
    read: Option<Weak<Mutex<Pipe>>>,
}

impl Delivery {
    /// Separate the guest-visible bytes from what an undelivered reply must undo.
    pub(super) fn split(self) -> (Vec<u8>, Journal) {
        let journal = Journal {
            issued: self.issued,
            read: self.read,
        };
        (self.bytes, journal)
    }
}

/// The publication journal of one reply: handles it issues and the stream it drained.
#[derive(Default)]
pub(super) struct Journal {
    issued: Vec<u64>,
    read: Option<Weak<Mutex<Pipe>>>,
}

/// Undo a reply the guest will never receive: release its handles, and poison the stream it
/// drained so later reads fail with "stream read was abandoned" instead of skipping data.
pub(super) fn abandon(state: &mut State, journal: Journal) -> wasmtime::Result<()> {
    if let Some(pipe) = journal.read.and_then(|pipe| pipe.upgrade()) {
        pipe.lock().expect("pipe").abandon();
    }
    rollback(state, journal.issued)
}

/// Receiver of one admitted task's encoded result or error message.
pub(super) type Completion = oneshot::Receiver<Result<Delivery, String>>;
/// Completion channel of one admitted task; `None` for Store-turn bindings.
pub(super) type Reply = Option<oneshot::Sender<Result<Delivery, String>>>;
/// Spawn the prepared upstream call on the Store's concurrent task set.
pub(super) type Spawn =
    Box<dyn FnOnce(StoreContextMut<'_, State>, Reply) -> wasmtime::Result<JoinHandle> + Send>;

/// One admitted asynchronous packet: its Store task, the borrows it holds, and its reply.
pub(super) type Admitted = (JoinHandle, Vec<Arc<Lease>>, Completion);

/// The encoded reply of a WIT-synchronous call whose upstream implementation is Rust-`async`.
pub(super) type Awaited<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = wasmtime::Result<Vec<u8>>> + Send + 'a>>;

/// Result of decoding and validating one generated call.
pub(super) enum Call<'a> {
    /// A WIT-synchronous result.
    Ready(Vec<u8>),
    /// A WIT-synchronous result awaited while the guest stays suspended on the synchronous
    /// import (upstream awaits address checks, socket readiness or a listener).
    Await(Awaited<'a>),
    /// A WIT `async func` that completes on a later Store turn, with the borrows it holds.
    Spawn(Spawn, Vec<Arc<Lease>>),
}

/// Per-Store standard WASI state.
pub(crate) struct Standard {
    handles: Handles,
    /// The private stream adapter of socket output streams (lazily instantiated).
    adapter: adapter::Adapter<State>,
    /// Interfaces imported by this Orb's checked contract; everything else is refused.
    allowed: BTreeSet<String>,
    /// An upstream trap quarantines this revision even if guest code catches its reply.
    trapped: Option<String>,
}

impl Standard {
    /// Admit only interfaces the Orb imported and was granted at installation.
    pub(crate) fn new(
        allowed: BTreeSet<String>,
        files: crate::engine::FileBudget,
        adapter: Arc<StreamAdapter>,
    ) -> Self {
        Self {
            handles: Handles::with_files(files),
            adapter: adapter::Adapter::new(adapter),
            allowed,
            trapped: None,
        }
    }
}

/// Whether this revision can run standard filesystem producers during a Store turn.
pub(super) fn uses_filesystem(state: &State) -> bool {
    state
        .standard
        .allowed
        .iter()
        .any(|interface| interface.starts_with("wasi:filesystem/"))
}

/// A malformed or unauthorized packet is refused before calling upstream.
#[derive(Debug)]
struct Refused(String);

impl std::fmt::Display for Refused {
    fn fmt(&self, format: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        format.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

/// Distinguish boundary refusals from upstream traps.
fn invalid(message: &str) -> wasmtime::Error {
    wasmtime::Error::new(Refused(message.to_owned()))
}

/// Reserve bounded filesystem admission before any upstream operation.
fn admit_file() -> wasmtime::Result<tokio::sync::OwnedSemaphorePermit> {
    crate::io::admit_file().map_err(|error| invalid(&error.to_string()))
}

/// Close the reply being encoded. Its journaled handles are published with the bytes, or, on a
/// boundary refusal (full table, oversized reply), released with every orphaned upstream
/// resource so a refused call leaves no hidden Store state. Other errors are upstream traps.
fn commit(
    state: &mut State,
    bytes: wasmtime::Result<Vec<u8>>,
) -> wasmtime::Result<Result<Delivery, String>> {
    let issued = std::mem::take(&mut state.standard.handles.issued);
    state.standard.handles.prepaid.clear();
    let refusal = state.standard.handles.refusal.take();
    let bytes = match (bytes, refusal) {
        (Ok(_), Some(refusal)) => Err(invalid(&refusal)),
        (Ok(bytes), None) if bytes.len() > state.packet_bytes => {
            Err(invalid("standard reply exceeds the packet limit"))
        }
        (result, _) => result,
    };
    match bytes {
        Ok(bytes) => Ok(Ok(Delivery {
            bytes,
            issued,
            read: None,
        })),
        Err(error) if error.is::<Refused>() => {
            rollback(state, issued)?;
            Ok(Err(error.to_string()))
        }
        Err(error) => Err(error),
    }
}

/// Release handles whose reply never reached the guest, plus orphans of a refused reply.
pub(super) fn rollback(state: &mut State, issued: Vec<u64>) -> wasmtime::Result<()> {
    for (kind, rep) in std::mem::take(&mut state.standard.handles.orphans) {
        drop_resource(kind, rep, state)?;
    }
    for id in issued {
        release(state, id)?;
    }
    Ok(())
}

/// Drop one guest-named handle. A lent resource is refused and stays owned; streams and futures
/// stay counted while upstream still holds their transmit state.
fn release(state: &mut State, id: u64) -> wasmtime::Result<()> {
    match state.standard.handles.remove(id)? {
        Entry::Resource { kind, rep, .. } => drop_resource(kind, rep, state),
        Entry::Stream(pipe) => {
            pipe.lock().expect("pipe").close();
            state.standard.handles.detach(id, pipe);
            Ok(())
        }
        Entry::Future(pending) => {
            pending.lock().expect("future").close();
            state.standard.handles.detach(id, pending);
            Ok(())
        }
        // An unwritten future resolves to the abandonment error for its upstream reader.
        Entry::Promise { state: promise, .. } => {
            promise.abandon();
            state.standard.handles.detach(id, promise);
            Ok(())
        }
        // Closing the writer ends the input stream after its buffered bytes are delivered.
        Entry::Writer(inbox) => {
            inbox.lock().expect("inbox").close();
            state.standard.handles.detach(id, inbox);
            Ok(())
        }
        Entry::Detached(_) => unreachable!("refused by remove"),
    }
}

/// Drain the typed close queue: store-drop resources close through upstream `WithStore` drops,
/// other resources through their `Host` drop, and unpublished handles are released. Closing may
/// queue more work (a released request handle), so this runs until the queue is empty.
pub(super) fn close_queued(mut store: StoreContextMut<'_, State>) -> wasmtime::Result<()> {
    adapter::close_unopened(store.as_context_mut())?;
    loop {
        let queued = std::mem::take(
            &mut *store
                .data()
                .standard
                .handles
                .closing
                .lock()
                .expect("closing"),
        );
        if queued.is_empty() {
            return Ok(());
        }
        for close in queued {
            match close {
                Close::Resource(kind, rep) if STORE_DROPPED[kind as usize] => {
                    drop_queued(kind, rep, store.as_context_mut())?
                }
                Close::Resource(kind, rep) => drop_resource(kind, rep, store.data_mut())?,
                Close::Handle(id) => match release(store.data_mut(), id) {
                    Err(error) if error.is::<Refused>() => {}
                    other => other?,
                },
            }
        }
    }
}

/// Publish a task result; an upstream trap is recorded for the Store owner, and a reply whose
/// callback owner is gone releases the handles nobody can name.
fn settle(
    access: &wasmtime::component::Accessor<State>,
    reply: Reply,
    bytes: wasmtime::Result<Vec<u8>>,
) {
    access.with(|mut store| {
        let state = store.data_mut();
        let delivery = match commit(state, bytes) {
            Ok(delivery) => delivery,
            Err(error) => {
                state.standard.trapped = Some(format!("{error:#}"));
                Err(format!("{error:#}"))
            }
        };
        let unpublished = match reply {
            Some(reply) => reply.send(delivery).err(),
            None => Some(delivery),
        };
        if let Some(Ok(unpublished)) = unpublished
            && let Err(error) = abandon(state, unpublished.split().1)
        {
            state.standard.trapped = Some(format!("{error:#}"));
        }
        if let Err(error) = close_queued(store.as_context_mut()) {
            store.data_mut().standard.trapped = Some(format!("{error:#}"));
        }
    });
}

/// Wrap a stream or future reply, refusing one larger than a standard packet. A stream read
/// names the pipe it drained so an undelivered reply poisons it.
fn bounded(
    bytes: Result<Vec<u8>, String>,
    limit: usize,
    read: Option<&Shared<Pipe>>,
) -> Result<Delivery, String> {
    match bytes {
        Ok(bytes) if bytes.len() > limit => Err("standard reply exceeds the packet limit".into()),
        Ok(bytes) => Ok(Delivery {
            bytes,
            issued: Vec::new(),
            read: read.map(Arc::downgrade),
        }),
        Err(error) => Err(error),
    }
}

/// Deliver one future value with the handles it issues; a value that cannot be delivered
/// (oversized, or its callback owner is gone) releases them through the close queue.
fn deliver_future(
    reply: Reply,
    (value, issued): (Result<Vec<u8>, String>, Vec<u64>),
    limit: usize,
    closing: &Shared<Vec<Close>>,
) {
    let delivery = bounded(value, limit, None).map(|mut delivery| {
        delivery.issued = issued.clone();
        delivery
    });
    let delivered = match (reply, delivery) {
        (Some(reply), Ok(delivery)) => reply.send(Ok(delivery)).is_ok(),
        (Some(reply), Err(error)) => {
            let _ = reply.send(Err(error));
            false
        }
        (None, _) => false,
    };
    if !delivered && !issued.is_empty() {
        let mut closing = closing.lock().expect("closing");
        closing.extend(issued.into_iter().map(Close::Handle));
    }
}

/// Refuse further guest execution after an upstream trap.
pub(super) fn check_trap(state: &mut State) -> wasmtime::Result<()> {
    match state.standard.trapped.take() {
        Some(error) => Err(wasmtime::Error::msg(error)),
        None => Ok(()),
    }
}

type Shared<T> = Arc<Mutex<T>>;

/// Decode the packet header and refuse methods outside the Orb's imports.
fn method(state: &State, reader: &mut Reader<'_>, asynchronous: bool) -> wasmtime::Result<u32> {
    let method = reader.u32()?;
    let interface = INTERFACES
        .get(method as usize)
        .ok_or_else(|| invalid("unknown standard method"))?;
    if !state.standard.allowed.contains(*interface) {
        return Err(invalid("standard interface is not imported by this Orb"));
    }
    if ASYNCHRONOUS[method as usize] != asynchronous {
        return Err(invalid("standard call uses the wrong calling convention"));
    }
    Ok(method)
}

/// Serve one WIT-synchronous standard packet while the guest stack waits. Almost every call
/// completes in its first poll; Rust-`async` upstream functions keep the guest suspended on its
/// fiber until they finish, which preserves WIT synchronous semantics.
pub(super) async fn synchronous(
    mut store: StoreContextMut<'_, State>,
    packet: &[u8],
) -> wasmtime::Result<Result<Vec<u8>, String>> {
    let result = async {
        let mut reader = Reader(packet.get(1..).unwrap_or_default());
        match reader.u8()? {
            CALL => {
                if !store.data().callback_scope.active() {
                    return Err(invalid("callback scope ended"));
                }
                let index = method(store.data(), &mut reader, false)?;
                let bytes = match dispatch(index, &mut reader, store.as_context_mut()) {
                    Ok(Call::Ready(bytes)) => Ok(bytes),
                    Ok(Call::Await(reply)) => reply.await,
                    Ok(Call::Spawn(..)) => Err(invalid("asynchronous call reached the sync path")),
                    Err(error) => Err(error),
                };
                // The synchronous reply is read by the waiting guest stack, so it publishes now.
                commit(store.data_mut(), bytes)?
                    .map(|delivery| delivery.bytes)
                    .map_err(|refusal| invalid(&refusal))
            }
            DROP => {
                let id = reader.u64()?;
                reader.end()?;
                release(store.data_mut(), id)?;
                Ok(Vec::new())
            }
            STREAM_NEW => {
                if !store.data().callback_scope.active() {
                    return Err(invalid("callback scope ended"));
                }
                let ty = reader.u32()?;
                reader.end()?;
                if ty != BYTE_STREAM {
                    return Err(invalid("stream type is not writable by the guest"));
                }
                let state = store.data_mut();
                let id = state.standard.handles.writer();
                let mut writer = Writer::default();
                writer.u64(*id.as_ref().unwrap_or(&0));
                commit(state, id.map(|_| writer.0))?
                    .map(|delivery| delivery.bytes)
                    .map_err(|refusal| invalid(&refusal))
            }
            FUTURE_NEW => {
                if !store.data().callback_scope.active() {
                    return Err(invalid("callback scope ended"));
                }
                let ty = reader.u32()?;
                reader.end()?;
                let state = store.data_mut();
                let id = state.standard.handles.promise(ty);
                let mut writer = Writer::default();
                writer.u64(*id.as_ref().unwrap_or(&0));
                commit(state, id.map(|_| writer.0))?
                    .map(|delivery| delivery.bytes)
                    .map_err(|refusal| invalid(&refusal))
            }
            FUTURE_WRITE => {
                if !store.data().callback_scope.active() {
                    return Err(invalid("callback scope ended"));
                }
                let id = reader.u64()?;
                let handles = &mut store.data_mut().standard.handles;
                let Some(Entry::Promise { ty, state }) = handles.entries.get(&id) else {
                    return Err(invalid("handle is not a live writable future"));
                };
                let (ty, state) = (*ty, Arc::clone(state));
                // Fully decoded and validated before acceptance; owned handles in the value
                // transfer only then. The id keeps naming the readable end (a write may come
                // before the transfer) until DROP or callback retire; a second write is refused.
                write_future(ty, state, &mut reader, handles)?;
                Ok(Vec::new())
            }
            _ => Err(invalid("unknown standard operation")),
        }
    }
    .await;
    close_queued(store.as_context_mut())?;
    match result {
        Ok(bytes) => Ok(Ok(bytes)),
        Err(error) if error.is::<Refused>() => Ok(Err(error.to_string())),
        Err(error) => Err(error),
    }
}

/// Serve a synchronous packet that must complete in its first poll (tests of calls that never
/// suspend).
#[cfg(test)]
pub(super) fn synchronous_now(
    store: StoreContextMut<'_, State>,
    packet: &[u8],
) -> wasmtime::Result<Result<Vec<u8>, String>> {
    futures::FutureExt::now_or_never(synchronous(store, packet))
        .expect("synchronous standard call completes in its first poll")
}

/// Admit one asynchronous standard packet as a Store task owned by the current callback.
pub(super) fn spawn(
    mut store: StoreContextMut<'_, State>,
    packet: &[u8],
) -> wasmtime::Result<Result<Admitted, String>> {
    let result = (|| -> wasmtime::Result<_> {
        if !store.data().callback_scope.active() {
            return Err(invalid("callback scope ended"));
        }
        let mut reader = Reader(packet.get(1..).unwrap_or_default());
        let (send, receive) = oneshot::channel();
        let reply = Some(send);
        let packet_bytes = store.data().packet_bytes;
        let mut leases = Vec::new();
        let handle = match reader.u8()? {
            CALL => {
                let index = method(store.data(), &mut reader, true)?;
                let (spawn, lent) = match dispatch(index, &mut reader, store.as_context_mut())? {
                    Call::Spawn(spawn, lent) => (spawn, lent),
                    _ => return Err(invalid("synchronous call reached the async path")),
                };
                leases = lent;
                spawn(store.as_context_mut(), reply)?
            }
            STREAM_READ => {
                let id = reader.u64()?;
                // Bytes for `stream<u8>`, whole items for typed streams; the reply (tag, u32
                // length/count, payload) always fits one packet.
                let limit = (reader.u32()? as usize).clamp(1, PIPE_BYTES);
                reader.end()?;
                let Some(Entry::Stream(pipe)) = store.data().standard.handles.entries.get(&id)
                else {
                    return Err(invalid("handle is not a live readable stream"));
                };
                let pipe = Arc::clone(pipe);
                let closing = Arc::clone(&store.data().standard.handles.closing);
                // Queued at admission, so concurrent reads of one stream are served in guest
                // order and an aborted, never polled read still leaves the queue.
                let turn = ReadTurn::begin(Arc::clone(&pipe));
                store.as_context_mut().spawn(
                    async move |_: &wasmtime::component::Accessor<State>| {
                        let (value, issued) = read_stream(turn, limit, packet_bytes).await;
                        // Handles of drained items are published with this reply; a reply that
                        // cannot be delivered releases them, and an undelivered drain poisons
                        // the stream instead of skipping its data.
                        let delivery =
                            bounded(value, packet_bytes, Some(&pipe)).map(|mut delivery| {
                                delivery.issued = issued.clone();
                                delivery
                            });
                        let delivered = match (reply, delivery) {
                            (Some(reply), Ok(delivery)) => {
                                let sent = reply.send(Ok(delivery)).is_ok();
                                if !sent {
                                    pipe.lock().expect("pipe").abandon();
                                }
                                sent
                            }
                            (Some(reply), Err(error)) => {
                                let _ = reply.send(Err(error));
                                false
                            }
                            (None, _) => false,
                        };
                        if !delivered && !issued.is_empty() {
                            let mut closing = closing.lock().expect("closing");
                            closing.extend(issued.into_iter().map(Close::Handle));
                        }
                        Ok(())
                    },
                )?
            }
            STREAM_WRITE => {
                let id = reader.u64()?;
                // Refused before any copy into host buffers: a chunk is at most one buffer.
                let chunk = reader.bytes()?;
                if chunk.len() > PIPE_BYTES {
                    return Err(invalid("stream write exceeds 65536 bytes"));
                }
                reader.end()?;
                let chunk = chunk.to_vec();
                let Some(Entry::Writer(inbox)) = store.data().standard.handles.entries.get(&id)
                else {
                    return Err(invalid("handle is not a live writable stream"));
                };
                // Claimed before the task exists, so an aborted task still frees the slot.
                let writing = Writing::begin(Arc::clone(inbox)).map_err(invalid)?;
                store.as_context_mut().spawn(
                    async move |_: &wasmtime::component::Accessor<State>| {
                        let value = writing.write(chunk).await;
                        if let Some(reply) = reply {
                            let _ = reply.send(bounded(value, packet_bytes, None));
                        }
                        Ok(())
                    },
                )?
            }
            FUTURE_READ => {
                let id = reader.u64()?;
                reader.end()?;
                let handles = &mut store.data_mut().standard.handles;
                let Some(Entry::Future(pending)) = handles.entries.get(&id) else {
                    return Err(invalid("handle is not a live future"));
                };
                // A future value is read once; the guest name is consumed now, while the entry
                // stays counted until the producer and this reader release it.
                let pending = Arc::clone(pending);
                let closing = Arc::clone(&handles.closing);
                handles.detach(id, pending.clone());
                store.as_context_mut().spawn(
                    async move |_: &wasmtime::component::Accessor<State>| {
                        let value = read_future(pending).await;
                        deliver_future(reply, value, packet_bytes, &closing);
                        Ok(())
                    },
                )?
            }
            _ => return Err(invalid("unknown standard operation")),
        };
        Ok((handle, leases, receive))
    })();
    close_queued(store.as_context_mut())?;
    match result {
        Ok(value) => Ok(Ok(value)),
        Err(error) if error.is::<Refused>() => Ok(Err(error.to_string())),
        Err(error) => Err(error),
    }
}

/// Start the adapter pumps of socket output streams bound since the last Store turn, so the turn
/// that serves their reads also feeds them.
pub(super) async fn start_streams(store: &mut wasmtime::Store<State>) -> wasmtime::Result<()> {
    if adapter::pending(store.data()) {
        adapter::start(store).await?;
    }
    // A socket that connects in this turn prepares its send writer in the adapter, so Orbs that
    // import sockets have it before their first turn; other Orbs never instantiate it.
    if uses_sockets(store.data()) {
        adapter::instantiate(store).await?;
    }
    Ok(())
}

/// Whether this revision imports TCP sockets (and so may send through the stream adapter).
fn uses_sockets(state: &State) -> bool {
    state.standard.allowed.contains("wasi:sockets/types@0.3.0")
}

/// Callback-owned Store tasks that may still be running; dropping the owner aborts them and ends
/// their borrows. Settled tasks are pruned, so a long callback's sequential calls stay bounded by
/// its outstanding effects rather than by every call it ever made.
#[derive(Default)]
pub(super) struct Tasks(Vec<(JoinHandle, Vec<Arc<Lease>>)>);

impl Tasks {
    /// Retain a callback task until it settles or its suspended work is revoked.
    pub(super) fn push(&mut self, handle: JoinHandle, leases: Vec<Arc<Lease>>) {
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        // A JoinHandle is ready only once its task future has been dropped.
        self.0.retain_mut(|(handle, _)| {
            std::future::Future::poll(std::pin::Pin::new(handle), &mut context).is_pending()
        });
        self.0.push((handle, leases));
    }

    /// Abort every task and end its borrows now. An aborted Store task is never polled again;
    /// the returned handles resolve once the Store drops its future on the next poll.
    pub(super) fn revoke(&mut self) -> Vec<JoinHandle> {
        std::mem::take(&mut self.0)
            .into_iter()
            .map(|(handle, leases)| {
                handle.abort();
                for lease in leases {
                    lease.release();
                }
                handle
            })
            .collect()
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.0.len()
    }
}

impl Drop for Tasks {
    fn drop(&mut self) {
        // An aborted task is never polled again, so its borrows end now rather than when the
        // Store next drops the future; guest cleanup may then drop the resource immediately.
        drop(self.revoke());
    }
}

/// Consecutive Store polls without settlement progress before retirement stops waiting; a
/// stalled upstream producer cannot hold the callback, while one that is still draining
/// accepted input keeps being polled.
const SETTLE_POLLS: usize = 8;

/// Dispose of a callback's standard work before the Orb goes idle. Aborted futures are dropped
/// by one Store turn without running, which returns their process-wide filesystem admission
/// permits and any reader state; closed writers flush their accepted bytes to upstream; finished
/// detached entries stop counting toward the table. This runs inside the callback's deadline.
/// If upstream stops making progress, unflushed input stays owned by the Store and flushes on
/// its next turn; disposing the revision first discards it. A callback that ends in an error or
/// interruption instead quarantines the Store, and dropping the Store drops every task.
pub(super) async fn retire(
    store: &mut wasmtime::Store<State>,
    tasks: &mut Tasks,
) -> wasmtime::Result<()> {
    let handles = tasks.revoke();
    // Aborted writes never accepted their chunk; open writers of the ending callback close now,
    // so accepted bytes reach upstream in this turn instead of waiting for a later callback.
    // Unwritten futures resolve to the abandonment error, so no upload commits truncated.
    store.data_mut().standard.handles.close_writers();
    store.data_mut().standard.handles.close_promises();
    // Bodies this callback read and did not finish close now. That close, and the resolver and
    // connect work of this callback's send tasks, end physically (sockets closed, lookups
    // returned, reactor connect futures dropped) before the callback retires.
    store.data().transport.commands.end();
    let store_work = store.data().transport.store_work.clone();
    // Streams this callback dropped whose pump has not finished closing them. Pumps of streams
    // the guest retains are revision resources and are not waited for.
    let closing = store.data().standard.adapter.closing.clone();
    if handles.is_empty()
        && !store.data_mut().standard.handles.purge()
        && closing.live() == 0
        && !adapter::stale(store.data_mut())
    {
        store_work.settled().await;
        return Ok(());
    }
    let files = uses_filesystem(store.data());
    let turn = store.run_concurrent(async |access| {
        futures::future::join_all(handles).await;
        // Each closing pump finishes inside this turn: its adapter `close`, its in-flight read
        // and the pump itself. The callback deadline bounds the wait.
        closing.settled().await;
        // Send writers prepared for sockets the guest dropped without sending close now.
        adapter::sweep(access).await?;
        // Settle until no detached host state remains. The remaining work (detached entries
        // plus accepted input bytes upstream has not consumed) never grows here, because no
        // guest code runs, so the loop ends after at most SETTLE_POLLS polls per unit of
        // progress; the callback deadline still bounds it in wall time.
        let mut least = usize::MAX;
        let mut stalled = 0;
        while let Some(remaining) = access.with(|mut store| {
            // Aborted tasks dropped their untaken owned arguments into the close queue.
            close_queued(store.as_context_mut())?;
            wasmtime::Result::<_>::Ok(store.data_mut().standard.handles.unsettled())
        })? {
            if remaining < least {
                least = remaining;
                stalled = 0;
            } else {
                stalled += 1;
                if stalled >= SETTLE_POLLS {
                    // Idle upstream state: never poll a later retirement for it again.
                    access.with(|mut store| store.data_mut().standard.handles.mark_stalled());
                    break;
                }
            }
            let mut yielded = false;
            std::future::poll_fn(|context| {
                if std::mem::replace(&mut yielded, true) {
                    std::task::Poll::Ready(())
                } else {
                    context.waker().wake_by_ref();
                    std::task::Poll::Pending
                }
            })
            .await;
        }
        wasmtime::Result::<()>::Ok(())
    });
    if files {
        // Woken producers may run qualified inline filesystem work in this turn.
        crate::io::file_turn(turn)
            .await
            .map_err(|error| wasmtime::Error::msg(error.to_string()))???;
    } else {
        turn.await??;
    }
    store_work.settled().await;
    Ok(())
}

#[cfg(test)]
mod filesystem_qualification;
#[cfg(test)]
mod http_qualification;
#[cfg(test)]
mod qualification;
#[cfg(test)]
mod sockets_qualification;
#[cfg(test)]
mod system_qualification;

#[cfg(test)]
mod tests {
    use super::*;
    use wasmtime::component::Resource;

    #[test]
    fn resource_identities_never_alias_another_store_or_a_retired_entry() {
        let mut first = Handles::new();
        let mut other = Handles::new();
        let id = first.own(0, Resource::<()>::new_own(7));
        let foreign = other.own(0, Resource::<()>::new_own(7));
        assert_ne!(id, foreign);
        assert!(first.borrow::<()>(0, foreign).is_err());
        assert!(other.borrow::<()>(0, id).is_err());
        assert!(first.borrow::<()>(1, id).is_err());
        assert_eq!(first.take::<()>(0, id).unwrap().rep(), 7);
        let replacement = first.own(0, Resource::<()>::new_own(7));
        assert_ne!(replacement, id);
        assert!(first.borrow::<()>(0, id).is_err());
        assert_eq!(first.borrow::<()>(0, replacement).unwrap().rep(), 7);
    }

    /// The browser and guest interpret the table generated with this dispatcher.
    #[test]
    fn typescript_table_matches_the_generated_bindings() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../packages/ts/sdk/src/interpreter/standard.ts"
        );
        let generated = include_str!(concat!(env!("OUT_DIR"), "/standard.ts"));
        if std::env::var_os("TEMBRA_BLESS").is_some() {
            std::fs::write(path, generated).unwrap();
        }
        assert_eq!(std::fs::read_to_string(path).unwrap(), generated);
    }
}
