//! The private stream adapter: a tiny guest component that holds socket streams on the guest
//! side. It reads output streams (`tcp-socket.receive` bytes, `tcp-socket.listen` sockets) and
//! writes the input stream upstream's `tcp-socket.send` consumes. Dropping a stream it holds is
//! the standard component-model drop, so upstream's producer or consumer closes even while the
//! peer is idle or does not read. A host-to-host pipe cannot do that: it is only polled when data
//! moves. One implementation serves the source interpreter's Store and the compiled component
//! Store ([`AdapterStore`]).
//!
//! Ownership rules (each is load-bearing):
//! - one revision-owned pump per stream is the only caller of the adapter for that stream; it is
//!   never aborted, and no adapter call it starts is ever dropped before it returns;
//! - the pump issues `close` itself after it observes the stream's close (a guest DROP, or the
//!   cancellation of the call that owns it), then keeps awaiting its in-flight call; it never
//!   starts a call after observing the close;
//! - from the close until the pump finished, the stream counts in the revision's closing ledger,
//!   which callback retirement and compiled call completion wait for; idle retained streams'
//!   pumps are revision resources and never delay either;
//! - a send writer is prepared for each connected or accepted socket by its own never-aborted
//!   task, because `tcp-socket.send` is WIT-synchronous and cannot call the adapter; it is
//!   published only for the socket it was prepared for;
//! - adapter ids are never reused within an instance, and a second read or write of a busy
//!   stream traps.
//!
//! Cancellation reporting: a stream or send the owning call's cancellation cut short reports
//! `other("call cancelled")` on its completion future; a real end, upstream errors and an
//! explicit DROP keep upstream's value.
//!
//! The component is compiled once per Engine on first use. A Store instantiates it only when its
//! Orb imports `wasi:sockets/types`: a source Store before its first turn, a compiled Store before
//! its component, so streams opened while the component initializes take the same route. Its
//! linear memory is its own and capped per memory by `StoreLimits`, separately from the
//! interpreter heap; a trap inside it (for example fuel exhaustion during a pump call) quarantines
//! the revision like any other trap.
use super::{
    PIPE_BYTES, State, Writer,
    handles::Close,
    streams::{Pipe, PipeConsumer},
};
use futures::future::{BoxFuture, Either, join, select};
use std::{
    future::poll_fn,
    pin::{Pin, pin},
    sync::{Arc, Mutex, OnceLock},
    task::{Context, Poll},
};
use wasmtime::{
    AsContextMut, StoreContextMut,
    component::{
        Accessor, Component, Destination, InstancePre, Linker, Resource, StreamProducer,
        StreamReader, StreamResult, TypedFunc,
    },
    error::Context as _,
};
use wasmtime_wasi::{
    p3::bindings::sockets::types::{ErrorCode, HostTcpSocket, HostTcpSocketWithStore},
    sockets::{TcpSocket, WasiSockets, WasiSocketsView},
};

/// The reviewed prebuilt component (`just build-stream-adapter`, checked byte-for-byte by
/// `just check-stream-adapter`), built from `crates/executor-wasm/stream-adapter`.
const COMPONENT: &[u8] = include_bytes!("stream_adapter.wasm");
/// SHA-256 of [`COMPONENT`].
#[cfg_attr(not(test), allow(dead_code))]
pub(super) const COMPONENT_SHA256: &str =
    "1d286ddc808110f7d83d181cd2c15a62a600b2c90c40817f0dfde7b6f5fd3566";

type Accepted = Option<Vec<Resource<TcpSocket>>>;

/// The completion future of a socket stream or send, as upstream and the guest see it.
pub(crate) type Completion = wasmtime::component::FutureReader<Result<(), ErrorCode>>;

/// A Store data type that runs adapter pumps: the source interpreter's Store and the compiled
/// component Store. Everything else (the pump, the close sequence, the ledgers) is shared.
pub(crate) trait AdapterStore: WasiSocketsView + Send + Sized + 'static {
    /// What one accepted socket holds while it waits for the guest (the source open-file unit).
    type Permit: Send + 'static;

    /// Whether `receive`/`listen` streams are adapted where `CallSockets` returns them (compiled
    /// Stores). The source Store adapts them in its generated dispatch instead.
    const GUEST_STREAMS: bool;

    /// The command boundary that owns streams and sends while a call or callback uses them.
    fn commands(&self) -> &crate::http::Commands;

    /// This Store's adapter state.
    fn adapter(&mut self) -> &mut Adapter<Self>;

    /// The Engine-wide link of the adapter for this Store type.
    fn linked(compiled: &Compiled) -> &OnceLock<Result<InstancePre<Self>, String>>;

    /// Wait for what one more accepted socket needs before it is pulled, if anything.
    fn accept_budget(&mut self) -> Option<BoxFuture<'static, Self::Permit>> {
        None
    }

    /// Refuse before pulling a socket that could not be delivered.
    fn accept_capacity(&mut self) -> Result<(), String> {
        Ok(())
    }

    /// Deliver one accepted socket as the next item of `pipe`. By default the item is the exact
    /// upstream resource, moved to the guest when it reads; unread, it closes through upstream.
    fn deliver(
        &mut self,
        pipe: &Arc<Mutex<Pipe>>,
        kind: u32,
        socket: Resource<TcpSocket>,
        _permit: Option<Self::Permit>,
    ) -> wasmtime::Result<()> {
        let rep = socket.rep();
        pipe.lock()
            .expect("pipe")
            .push_owned(rep.to_le_bytes().to_vec(), vec![Close::Resource(kind, rep)]);
        Ok(())
    }

    /// Close a socket nobody will receive. Its connection closes now: upstream's resource drop,
    /// never a forgotten table index.
    fn close_socket(&mut self, _kind: u32, socket: Resource<TcpSocket>) -> wasmtime::Result<()> {
        HostTcpSocket::drop(&mut self.sockets(), socket)
    }
}

/// Typed exports of one adapter instance.
#[derive(Clone, Copy)]
pub(super) struct Exports {
    pub(super) open_bytes: TypedFunc<(StreamReader<u8>,), (u32,)>,
    pub(super) read: TypedFunc<(u32, u32), (Option<Vec<u8>>,)>,
    close_bytes: TypedFunc<(u32,), ()>,
    pub(super) open_sockets: TypedFunc<(StreamReader<Resource<TcpSocket>>,), (u32,)>,
    pub(super) accept: TypedFunc<(u32, u32), (Accepted,)>,
    pub(super) close_sockets: TypedFunc<(u32,), ()>,
    open_writer: TypedFunc<(), ((u32, StreamReader<u8>),)>,
    write: TypedFunc<(u32, Vec<u8>), (bool,)>,
    close_writer: TypedFunc<(u32,), ()>,
}

impl Exports {
    fn resolve<T: 'static>(
        instance: &wasmtime::component::Instance,
        mut store: impl AsContextMut<Data = T>,
    ) -> wasmtime::Result<Self> {
        let mut index = |interface: &str, name: &str| {
            let interface = instance
                .get_export_index(&mut store, None, interface)
                .with_context(|| format!("stream adapter exports {interface}"))?;
            instance
                .get_export_index(&mut store, Some(&interface), name)
                .with_context(|| format!("stream adapter exports {name}"))
        };
        let bytes = "tembra:stream-adapter/bytes@0.1.0";
        let sockets = "tembra:stream-adapter/sockets@0.1.0";
        let writes = "tembra:stream-adapter/writes@0.1.0";
        let indices = [
            index(bytes, "open")?,
            index(bytes, "read")?,
            index(bytes, "close")?,
            index(sockets, "open")?,
            index(sockets, "accept")?,
            index(sockets, "close")?,
            index(writes, "open")?,
            index(writes, "write")?,
            index(writes, "close")?,
        ];
        Ok(Self {
            open_bytes: instance.get_typed_func(&mut store, indices[0])?,
            read: instance.get_typed_func(&mut store, indices[1])?,
            close_bytes: instance.get_typed_func(&mut store, indices[2])?,
            open_sockets: instance.get_typed_func(&mut store, indices[3])?,
            accept: instance.get_typed_func(&mut store, indices[4])?,
            close_sockets: instance.get_typed_func(&mut store, indices[5])?,
            open_writer: instance.get_typed_func(&mut store, indices[6])?,
            write: instance.get_typed_func(&mut store, indices[7])?,
            close_writer: instance.get_typed_func(&mut store, indices[8])?,
        })
    }
}

/// The adapter compiled once per Engine on first use, and linked once per Engine and Store type.
#[derive(Default)]
pub(crate) struct Compiled {
    component: OnceLock<Result<Component, String>>,
    pub(super) source: OnceLock<Result<InstancePre<State>, String>>,
    pub(crate) compiled: OnceLock<Result<InstancePre<crate::component::ComponentState>, String>>,
}

impl Compiled {
    fn get<T: AdapterStore>(&self, engine: &wasmtime::Engine) -> wasmtime::Result<InstancePre<T>> {
        T::linked(self)
            .get_or_init(|| {
                (|| {
                    let component = self
                        .component
                        .get_or_init(|| {
                            Component::new(engine, COMPONENT).map_err(|error| format!("{error:#}"))
                        })
                        .clone()
                        .map_err(wasmtime::Error::msg)?;
                    let mut linker = Linker::<T>::new(engine);
                    // The adapter imports only the `tcp-socket` resource type; upstream's own
                    // definition gives accepted sockets their exact upstream drop.
                    wasmtime_wasi::p3::bindings::sockets::types::add_to_linker::<T, WasiSockets>(
                        &mut linker,
                        T::sockets,
                    )?;
                    linker.instantiate_pre(&component)
                })()
                .map_err(|error: wasmtime::Error| format!("stream adapter: {error:#}"))
            })
            .clone()
            .map_err(wasmtime::Error::msg)
    }
}

/// An upstream output stream bound to a guest stream handle, before its pump starts.
enum Source {
    Bytes(StreamReader<u8>),
    /// Accepted sockets, issued as items of resource kind `u32`.
    Sockets(StreamReader<Resource<TcpSocket>>, u32),
    /// The input of a `tcp-socket.send`, forwarded into the adapter writer `u32` whose reader
    /// upstream's send consumes.
    Send(StreamReader<u8>, u32),
}

/// Per-Store adapter state.
pub(crate) struct Adapter<T> {
    compiled: Arc<Compiled>,
    exports: Option<Exports>,
    /// Source streams bound since the last Store turn; their pumps start before the next turn.
    unopened: Vec<(Source, PipeConsumer)>,
    /// Pumped streams the guest dropped whose close sequence has not physically finished.
    pub(crate) closing: crate::http::Ledger,
    /// Compiled stream items nobody will read (accepted sockets), closed where the Store is held.
    released: Arc<Mutex<Vec<Close>>>,
    /// The one guest writer of each connected or accepted socket, by socket rep.
    prepared: std::collections::HashMap<u32, Prepared>,
    /// Identity of the latest preparation, so a superseded one never publishes its writer.
    preparations: u64,
    /// Prepared writers whose socket is gone, closed at the next retirement or call end.
    retired: Vec<(u32, StreamReader<u8>)>,
    /// Preparations that finished after their entry was superseded or forgotten (tests).
    #[cfg(test)]
    discarded: usize,
    _store: std::marker::PhantomData<fn(T)>,
}

impl<T> Adapter<T> {
    pub(crate) fn new(compiled: Arc<Compiled>) -> Self {
        Self {
            compiled,
            exports: None,
            unopened: Vec::new(),
            closing: crate::http::Ledger::default(),
            released: Arc::default(),
            prepared: Default::default(),
            preparations: 0,
            retired: Vec::new(),
            #[cfg(test)]
            discarded: 0,
            _store: std::marker::PhantomData,
        }
    }
}

impl AdapterStore for State {
    type Permit = super::handles::FilePermit;

    const GUEST_STREAMS: bool = false;

    fn commands(&self) -> &crate::http::Commands {
        &self.transport.commands
    }

    fn adapter(&mut self) -> &mut Adapter<Self> {
        &mut self.standard.adapter
    }

    fn linked(compiled: &Compiled) -> &OnceLock<Result<InstancePre<Self>, String>> {
        &compiled.source
    }

    /// One unit of the open-file budget, awaited (racing the guest's DROP) before pulling.
    fn accept_budget(&mut self) -> Option<BoxFuture<'static, Self::Permit>> {
        Some(Box::pin(self.standard.handles.file_waiter()))
    }

    /// A full handle table refuses the stream before a peer is accepted.
    fn accept_capacity(&mut self) -> Result<(), String> {
        self.standard
            .handles
            .reserve(1)
            .map_err(|error| error.to_string())
    }

    fn deliver(
        &mut self,
        pipe: &Arc<Mutex<Pipe>>,
        kind: u32,
        socket: Resource<TcpSocket>,
        permit: Option<Self::Permit>,
    ) -> wasmtime::Result<()> {
        issue(self, pipe, kind, socket, permit)
    }

    fn close_socket(&mut self, kind: u32, socket: Resource<TcpSocket>) -> wasmtime::Result<()> {
        super::drop_resource(kind, socket.rep(), self)
    }
}

/// Bind a `tcp-socket.receive` byte stream to its guest handle.
pub(super) fn bytes(
    mut store: StoreContextMut<'_, State>,
    reader: StreamReader<u8>,
    pipe: PipeConsumer,
) {
    let adapter = &mut store.data_mut().standard.adapter;
    adapter.unopened.push((Source::Bytes(reader), pipe));
}

/// Bind a `tcp-socket.listen` stream to its guest handle; unread accepted sockets are released
/// through the close queue.
pub(super) fn sockets(
    mut store: StoreContextMut<'_, State>,
    reader: StreamReader<Resource<TcpSocket>>,
    pipe: PipeConsumer,
    kind: u32,
) {
    let standard = &mut store.data_mut().standard;
    pipe.0
        .lock()
        .expect("pipe")
        .release_into(Arc::clone(&standard.handles.closing));
    standard
        .adapter
        .unopened
        .push((Source::Sockets(reader, kind), pipe));
}

/// Close streams the guest dropped before their pump started: no adapter is involved yet, so the
/// readable end closes directly (dropping the upstream producer).
pub(super) fn close_unopened(mut store: StoreContextMut<'_, State>) -> wasmtime::Result<()> {
    let unopened = std::mem::take(&mut store.data_mut().standard.adapter.unopened);
    let mut kept = Vec::new();
    for (mut source, pipe) in unopened {
        if !pipe.0.lock().expect("pipe").closed() {
            kept.push((source, pipe));
            continue;
        }
        match &mut source {
            Source::Bytes(reader) => reader.close(store.as_context_mut())?,
            Source::Sockets(reader, _) => reader.close(store.as_context_mut())?,
            Source::Send(..) => unreachable!("sends start their pump at once"),
        }
    }
    store.data_mut().standard.adapter.unopened.extend(kept);
    Ok(())
}

/// Name the private stream adapter in a trap raised inside it. A source Store runs only two
/// kinds of Wasm, the interpreter module and the adapter, so a trap whose innermost frame is not
/// in the interpreter is the adapter's. The revision is quarantined either way.
pub(crate) fn attribute(
    error: wasmtime::Error,
    interpreter: Option<&wasmtime::Module>,
) -> wasmtime::Error {
    let in_adapter = error
        .downcast_ref::<wasmtime::WasmBacktrace>()
        .and_then(|backtrace| backtrace.frames().first())
        .is_some_and(|frame| {
            !interpreter.is_some_and(|module| wasmtime::Module::same(frame.module(), module))
        });
    if in_adapter {
        error.context("private stream adapter trapped; revision quarantined")
    } else {
        error
    }
}

/// Whether streams are waiting for their pumps.
pub(super) fn pending(state: &State) -> bool {
    !state.standard.adapter.unopened.is_empty()
}

/// The adapter's exports in this Store, instantiating it on first use.
async fn exports<T: AdapterStore>(store: &mut wasmtime::Store<T>) -> wasmtime::Result<Exports> {
    if let Some(exports) = store.data_mut().adapter().exports {
        return Ok(exports);
    }
    let engine = store.engine().clone();
    let pre = store.data_mut().adapter().compiled.get::<T>(&engine)?;
    let instance = pre
        .instantiate_async(&mut *store)
        .await
        .context("instantiate the private stream adapter")?;
    let exports = Exports::resolve(&instance, &mut *store)?;
    store.data_mut().adapter().exports = Some(exports);
    Ok(exports)
}

/// Instantiate the adapter in a Store whose Orb imports sockets, before any socket exists: a
/// compiled call is one Store turn, so a stream it opens must get its pump within that turn, and
/// a socket that connects must find the adapter to prepare its send writer.
pub(crate) async fn instantiate<T: AdapterStore>(
    store: &mut wasmtime::Store<T>,
) -> wasmtime::Result<()> {
    exports(store).await.map(drop)
}

/// Start a pump for every newly bound source stream, instantiating the adapter in this Store on
/// first use. Runs with the Store held outside a turn, right before the turn that polls the
/// pumps.
pub(super) async fn start(store: &mut wasmtime::Store<State>) -> wasmtime::Result<()> {
    close_unopened(store.as_context_mut())?;
    if !pending(store.data()) {
        return Ok(());
    }
    let exports = exports(store).await?;
    for (source, pipe) in std::mem::take(&mut store.data_mut().standard.adapter.unopened) {
        spawn(store.as_context_mut(), exports, source, pipe)?;
    }
    Ok(())
}

/// Start the one pump of a stream. The JoinHandle is dropped: that never aborts the pump, which
/// ends on its own after its stream ended or was closed.
fn spawn<T: AdapterStore>(
    mut store: StoreContextMut<'_, T>,
    exports: Exports,
    source: Source,
    pipe: PipeConsumer,
) -> wasmtime::Result<()> {
    let ledger = store.data_mut().adapter().closing.clone();
    pipe.0.lock().expect("pipe").attach_pump(&ledger);
    drop(
        store.spawn(async move |access: &Accessor<T>| -> wasmtime::Result<()> {
            let pumped = match source {
                Source::Bytes(reader) => pump_bytes(access, exports, reader, &pipe).await,
                Source::Sockets(reader, kind) => {
                    pump_sockets(access, exports, reader, &pipe, kind).await
                }
                Source::Send(data, writer) => pump_send(access, exports, data, writer, &pipe).await,
            };
            pumped.context("private stream adapter")?;
            // Unread compiled items of this (or an earlier) closed stream close now.
            access.with(|mut store| release(store.data_mut()))?;
            // Physical acknowledgement: the close (if any), the last read and the pump are all
            // done. The consumer's drop ends the stream for its readers.
            let finished = pipe.0.lock().expect("pipe").pump_finished();
            drop(pipe);
            drop(finished);
            Ok(())
        })?,
    );
    Ok(())
}

/// Close compiled stream items nobody will read, through upstream's resource drop.
pub(crate) fn release<T: AdapterStore>(state: &mut T) -> wasmtime::Result<()> {
    let released = std::mem::take(&mut *state.adapter().released.lock().expect("released"));
    for close in released {
        if let Close::Resource(kind, rep) = close {
            state.close_socket(kind, Resource::new_own(rep))?;
        }
    }
    Ok(())
}

/// Wait for room for the next chunk (or one item), or `None` once the guest dropped the stream.
async fn room(pipe: &Arc<Mutex<Pipe>>, items: bool) -> Option<usize> {
    poll_fn(|context| pipe.lock().expect("pipe").poll_room(context, items)).await
}

/// Ready once the guest dropped the stream.
async fn closed(pipe: &Arc<Mutex<Pipe>>) {
    poll_fn(|context| pipe.lock().expect("pipe").poll_closed(context)).await
}

/// Pump one byte stream: read chunks while the pipe has room; on DROP close, then let the
/// in-flight read (if any) return.
async fn pump_bytes<T: AdapterStore>(
    access: &Accessor<T>,
    exports: Exports,
    reader: StreamReader<u8>,
    pipe: &PipeConsumer,
) -> wasmtime::Result<()> {
    let pipe = &pipe.0;
    let (id,) = exports
        .open_bytes
        .call_concurrent(access, (reader,))
        .await?;
    loop {
        let Some(room) = room(pipe, false).await else {
            return exports.close_bytes.call_concurrent(access, (id,)).await;
        };
        let max = room.min(PIPE_BYTES) as u32;
        let mut read = pin!(exports.read.call_concurrent(access, (id, max)));
        let chunk = match select(read.as_mut(), pin!(closed(pipe))).await {
            Either::Left((read, _)) => read?.0,
            Either::Right(((), _)) => {
                let (close, read) =
                    join(exports.close_bytes.call_concurrent(access, (id,)), read).await;
                close?;
                read?;
                return Ok(());
            }
        };
        match chunk {
            Some(chunk) => pipe.lock().expect("pipe").push_bytes(&chunk),
            // End of stream; the adapter already released the id.
            None => return Ok(()),
        }
    }
}

/// Pump one stream of accepted sockets, one at a time: the next socket is pulled only after the
/// previous item was read and whatever an accepted socket holds is reserved for it.
async fn pump_sockets<T: AdapterStore>(
    access: &Accessor<T>,
    exports: Exports,
    reader: StreamReader<Resource<TcpSocket>>,
    pipe: &PipeConsumer,
    kind: u32,
) -> wasmtime::Result<()> {
    let pipe = &pipe.0;
    let (id,) = exports
        .open_sockets
        .call_concurrent(access, (reader,))
        .await?;
    let close = || exports.close_sockets.call_concurrent(access, (id,));
    loop {
        if room(pipe, true).await.is_none() {
            return close().await;
        }
        let permit = match access.with(|mut store| store.data_mut().accept_budget()) {
            Some(waiter) => match select(waiter, pin!(closed(pipe))).await {
                Either::Left((permit, _)) => Some(permit),
                Either::Right(((), _)) => return close().await,
            },
            None => None,
        };
        if let Err(error) = access.with(|mut store| store.data_mut().accept_capacity()) {
            pipe.lock().expect("pipe").fail(error);
            return close().await;
        }
        let mut read = pin!(exports.accept.call_concurrent(access, (id, 1)));
        let accepted = match select(read.as_mut(), pin!(closed(pipe))).await {
            Either::Left((read, _)) => read?.0,
            Either::Right(((), _)) => return close_mid_accept(access, kind, close(), read).await,
        };
        let Some(sockets) = accepted else {
            return Ok(());
        };
        let mut permit = permit;
        for socket in sockets {
            // An accepted socket can send at once: prepare its writer before delivering it.
            prepare(access, socket.rep()).await?;
            access.with(|mut store| store.data_mut().deliver(pipe, kind, socket, permit.take()))?;
        }
    }
}

/// The guest dropped a listen stream while the pump's `accept` was in flight: `close` the adapter
/// stream and let that `accept` return (it is never abandoned). Sockets the adapter took after
/// it saw the close are dropped inside the adapter. If its read completed first, `accept` hands
/// the socket back here instead. A host `Resource` is only a table index, so it is dropped
/// through upstream now: its connection closes before this returns, and so before the pump
/// releases its closing unit, never at revision teardown.
pub(super) async fn close_mid_accept<T: AdapterStore>(
    access: &Accessor<T>,
    kind: u32,
    close: impl Future<Output = wasmtime::Result<()>>,
    read: impl Future<Output = wasmtime::Result<(Accepted,)>>,
) -> wasmtime::Result<()> {
    let (closed, read) = join(close, read).await;
    if let (Some(sockets),) = read? {
        access.with(|mut store| {
            sockets
                .into_iter()
                .try_for_each(|socket| store.data_mut().close_socket(kind, socket))
        })?;
    }
    closed
}

/// Issue one accepted socket as an item of `pipe`. A refused issue (a full table) drops the
/// socket through upstream at once, closing that connection, and the stream keeps going.
pub(super) fn issue(
    state: &mut State,
    pipe: &Arc<Mutex<Pipe>>,
    kind: u32,
    socket: Resource<TcpSocket>,
    permit: Option<super::handles::FilePermit>,
) -> wasmtime::Result<()> {
    let handles = &mut state.standard.handles;
    debug_assert!(handles.issued.is_empty());
    handles.prepaid.extend(permit);
    let mut writer = Writer::default();
    writer.u64(handles.own(kind, socket));
    handles.prepaid.clear();
    let issued = std::mem::take(&mut handles.issued);
    if handles.refusal.take().is_some() {
        super::rollback(state, issued)?;
        return Ok(());
    }
    pipe.lock().expect("pipe").push_item(writer.0, issued);
    Ok(())
}

/// The current command owns `pipe` (a stream it reads, or a send it feeds) unless it already
/// registered in this command; its cancellation closes it, and a registration after the
/// cancellation closes it at once.
fn own(commands: &crate::http::Commands, pipe: &Arc<Mutex<Pipe>>, registered: &mut Option<u64>) {
    let generation = commands.generation();
    if *registered != Some(generation) {
        *registered = Some(generation);
        let owned: std::sync::Weak<dyn crate::http::CallOwned> = Arc::downgrade(pipe) as _;
        commands.read(owned);
    }
}

/// A compiled socket stream's state as seen by its guest reader.
struct Guest {
    pipe: Arc<Mutex<Pipe>>,
    /// The command this stream last registered with.
    registered: Option<u64>,
}

impl Guest {
    /// A read in a call this stream was not yet registered with: that call now owns it, so its
    /// cancellation closes it (a read after the cancellation closes it at once).
    fn register(&mut self, commands: &crate::http::Commands) {
        own(commands, &self.pipe, &mut self.registered);
    }

    /// The guest's zero-length read: ready once a read would complete.
    fn ready(
        &self,
        context: &mut Context<'_>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        let mut pipe = self.pipe.lock().expect("pipe");
        if pipe.readable() {
            Poll::Ready(Ok(StreamResult::Completed))
        } else if finish {
            Poll::Ready(Ok(StreamResult::Cancelled))
        } else {
            pipe.park_guest(context);
            Poll::Pending
        }
    }
}

impl Drop for Guest {
    /// The guest dropped its stream: close it, so its pump closes the upstream producer.
    fn drop(&mut self) {
        self.pipe.lock().expect("pipe").close();
    }
}

/// The guest-facing producer of a compiled `tcp-socket.receive` byte stream.
struct GuestBytes(Guest);

impl<T: AdapterStore> StreamProducer<T> for GuestBytes {
    type Item = u8;
    type Buffer = Option<u8>;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        mut store: StoreContextMut<'a, T>,
        destination: Destination<'a, u8, Option<u8>>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        let guest = &mut self.0;
        guest.register(store.data().commands());
        if destination.remaining(store.as_context_mut())? == Some(0) {
            return guest.ready(context, finish);
        }
        let mut destination = destination.as_direct(store, PIPE_BYTES);
        let room = destination.remaining()?;
        let poll = guest
            .pipe
            .lock()
            .expect("pipe")
            .poll_bytes(context, room.len());
        match poll {
            Poll::Ready(Some(chunk)) => {
                room[..chunk.len()].copy_from_slice(&chunk);
                destination.mark_written(chunk.len());
                Poll::Ready(Ok(StreamResult::Completed))
            }
            Poll::Ready(None) => Poll::Ready(Ok(StreamResult::Dropped)),
            Poll::Pending if finish => Poll::Ready(Ok(StreamResult::Cancelled)),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// The guest-facing producer of a compiled `tcp-socket.listen` stream: each item is the exact
/// upstream socket resource, moved to the guest.
struct GuestSockets(Guest);

impl<T: AdapterStore> StreamProducer<T> for GuestSockets {
    type Item = Resource<TcpSocket>;
    type Buffer = Option<Resource<TcpSocket>>;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        mut store: StoreContextMut<'a, T>,
        mut destination: Destination<'a, Resource<TcpSocket>, Option<Resource<TcpSocket>>>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        let guest = &mut self.0;
        guest.register(store.data().commands());
        if destination.remaining(store.as_context_mut())? == Some(0) {
            return guest.ready(context, finish);
        }
        let poll = guest.pipe.lock().expect("pipe").poll_item(context);
        match poll {
            Poll::Ready(Some(item)) => {
                let rep = u32::from_le_bytes(item.try_into().expect("a socket item is its rep"));
                destination.set_buffer(Some(Resource::new_own(rep)));
                Poll::Ready(Ok(StreamResult::Completed))
            }
            Poll::Ready(None) => Poll::Ready(Ok(StreamResult::Dropped)),
            Poll::Pending if finish => Poll::Ready(Ok(StreamResult::Cancelled)),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// The send writer of one socket.
enum Prepared {
    /// Its preparation (identity `u64`) is running in its own Store task.
    Pending(u64),
    /// The adapter writer id and the guest-produced reader `send` hands to upstream.
    Ready(u32, StreamReader<u8>),
}

/// Prepare the one guest writer of a socket that can now send (after a successful connect, or
/// before an accepted socket is delivered). `tcp-socket.send` is WIT-synchronous: there the
/// adapter cannot be called, yet upstream needs its input stream before the socket borrow ends.
///
/// The private `writes.open` call runs in its own Store task, which is never aborted, so a caller
/// aborted while waiting (a cancelled source callback's connect) never abandons it. The task
/// publishes the writer only if this preparation is still the socket's current one; otherwise
/// (the socket was dropped, or a later preparation replaced it) it closes the writer. It counts as
/// pending work in the closing ledger, so retirement and call completion wait for it.
pub(crate) async fn prepare<T: AdapterStore, D: wasmtime::component::HasData>(
    access: &Accessor<T, D>,
    rep: u32,
) -> wasmtime::Result<()> {
    let (prepared, ready) = tokio::sync::oneshot::channel();
    access.with(|mut store| -> wasmtime::Result<()> {
        let mut store = store.as_context_mut();
        let adapter = store.data_mut().adapter();
        // A Store whose Orb imports sockets instantiates the adapter before any socket exists.
        let exports = adapter.exports.ok_or_else(|| {
            wasmtime::Error::msg("the private stream adapter was not instantiated")
        })?;
        adapter.preparations += 1;
        let token = adapter.preparations;
        let unit = adapter.closing.hold();
        if let Some(Prepared::Ready(writer, reader)) =
            adapter.prepared.insert(rep, Prepared::Pending(token))
        {
            adapter.retired.push((writer, reader));
        }
        drop(
            store.spawn(async move |access: &Accessor<T>| -> wasmtime::Result<()> {
                let ((writer, reader),) = exports.open_writer.call_concurrent(access, ()).await?;
                let unclaimed = access.with(|mut store| {
                    let adapter = store.data_mut().adapter();
                    match adapter.prepared.get(&rep) {
                        Some(Prepared::Pending(current)) if *current == token => {
                            adapter
                                .prepared
                                .insert(rep, Prepared::Ready(writer, reader));
                            None
                        }
                        _ => {
                            #[cfg(test)]
                            {
                                adapter.discarded += 1;
                            }
                            Some((writer, reader))
                        }
                    }
                });
                if let Some((writer, mut reader)) = unclaimed {
                    access.with(|mut store| reader.close(&mut store))?;
                    exports
                        .close_writer
                        .call_concurrent(access, (writer,))
                        .await?;
                }
                drop(unit);
                let _ = prepared.send(());
                Ok(())
            })?,
        );
        Ok(())
    })?;
    // An aborted caller stops waiting here; the preparation completes in its own task.
    let _ = ready.await;
    Ok(())
}

/// The guest dropped a socket: forget its send writer. A ready writer is closed at the next
/// retirement or call end; a pending preparation closes its writer when it finishes.
pub(crate) fn forget<T: AdapterStore>(state: &mut T, rep: u32) {
    let adapter = state.adapter();
    if let Some(Prepared::Ready(writer, reader)) = adapter.prepared.remove(&rep) {
        adapter.retired.push((writer, reader));
    }
}

/// Whether ready writers of dropped sockets wait to be closed.
pub(crate) fn stale<T: AdapterStore>(state: &mut T) -> bool {
    let reps: Vec<u32> = state.adapter().prepared.keys().copied().collect();
    let retired = !state.adapter().retired.is_empty();
    let sockets = state.sockets();
    retired
        || reps.into_iter().any(|rep| {
            sockets
                .table
                .get(&Resource::<TcpSocket>::new_borrow(rep))
                .is_err()
        })
}

/// Close the writers of sockets the guest dropped without sending: forgotten ones, and ready ones
/// whose rep no longer holds a socket. Runs only in never-aborted retirement or call completion.
pub(crate) async fn sweep<T: AdapterStore, D: wasmtime::component::HasData>(
    access: &Accessor<T, D>,
) -> wasmtime::Result<()> {
    let (exports, swept) = access.with(|mut store| {
        let mut store = store.as_context_mut();
        let state = store.data_mut();
        let reps: Vec<u32> = state.adapter().prepared.keys().copied().collect();
        let gone: Vec<u32> = {
            let sockets = state.sockets();
            reps.into_iter()
                .filter(|rep| {
                    sockets
                        .table
                        .get(&Resource::<TcpSocket>::new_borrow(*rep))
                        .is_err()
                })
                .collect()
        };
        for rep in gone {
            forget(state, rep);
        }
        let adapter = state.adapter();
        (adapter.exports, std::mem::take(&mut adapter.retired))
    });
    let Some(exports) = exports else {
        return Ok(());
    };
    for (writer, mut reader) in swept {
        access.with(|mut store| reader.close(store.as_context_mut()))?;
        exports
            .close_writer
            .call_concurrent(access, (writer,))
            .await?;
    }
    Ok(())
}

/// `tcp-socket.send`: upstream's real `send` consumes the socket's prepared guest-produced
/// stream, and a pump forwards `data` into it. The current command owns the send: its
/// cancellation releases a writer blocked on `data`, ends upstream's consumer physically even
/// while the peer does not read, and reports `other("call cancelled")` on the returned future.
///
/// Every connected or accepted socket has a ready writer here, because `connect` and the accept
/// pump wait for its preparation. A socket without one never connected, or already sent: upstream
/// answers that call with its standard error and sends nothing.
pub(crate) fn send<T: AdapterStore>(
    mut store: StoreContextMut<'_, T>,
    socket: Resource<TcpSocket>,
    data: StreamReader<u8>,
) -> wasmtime::Result<Completion> {
    let prepared = store.data_mut().adapter().prepared.remove(&socket.rep());
    let exports = store.data_mut().adapter().exports;
    let upstream = |store: StoreContextMut<'_, T>, input| {
        <WasiSockets as HostTcpSocketWithStore<T>>::send(
            wasmtime::component::Access::<T, WasiSockets>::new(store, T::sockets),
            socket,
            input,
        )
    };
    let (writer, input, exports) = match (prepared, exports) {
        (Some(Prepared::Ready(writer, input)), Some(exports)) => (writer, input, exports),
        (Some(Prepared::Pending(_)), _) => {
            return Err(wasmtime::Error::msg(
                "a socket's send writer is still being prepared after its connect returned",
            ));
        }
        _ => return upstream(store, data),
    };
    let sent = upstream(store.as_context_mut(), input)?;
    let pipe: Arc<Mutex<Pipe>> = Arc::default();
    let mut registered = None;
    own(store.data().commands(), &pipe, &mut registered);
    let sent = completion(store.as_context_mut(), sent, &pipe)?;
    spawn(
        store,
        exports,
        Source::Send(data, writer),
        PipeConsumer(pipe, None),
    )?;
    Ok(sent)
}

/// Upstream's completion future of a socket stream, as the guest sees it: a stream its call's
/// cancellation cut short reports `other("call cancelled")` instead of upstream's clean `ok`,
/// because the host ended it. End of stream, upstream errors and an explicit DROP keep upstream's
/// own value.
fn completion<T: AdapterStore>(
    mut store: StoreContextMut<'_, T>,
    upstream: Completion,
    pipe: &Arc<Mutex<Pipe>>,
) -> wasmtime::Result<Completion> {
    let outcome: Arc<Mutex<Outcome>> = Arc::default();
    upstream.pipe(store.as_context_mut(), Relay(Arc::clone(&outcome)))?;
    wasmtime::component::FutureReader::new(
        store,
        Reported {
            outcome,
            pipe: Arc::clone(pipe),
        },
    )
}

/// Upstream's completion value, once it resolved.
#[derive(Default)]
struct Outcome {
    value: Option<Result<(), ErrorCode>>,
    reader: Option<std::task::Waker>,
}

/// Host consumer of upstream's completion future.
struct Relay(Arc<Mutex<Outcome>>);

impl<T: 'static> wasmtime::component::FutureConsumer<T> for Relay {
    type Item = Result<(), ErrorCode>;

    fn poll_consume(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        mut store: StoreContextMut<'_, T>,
        mut source: wasmtime::component::Source<'_, Self::Item>,
        finish: bool,
    ) -> Poll<wasmtime::Result<()>> {
        let mut value = None;
        source.read(store.as_context_mut(), &mut value)?;
        let mut outcome = self.0.lock().expect("outcome");
        match value {
            Some(value) => outcome.value = Some(value),
            None if finish => {}
            None => return Poll::Pending,
        }
        if let Some(reader) = outcome.reader.take() {
            reader.wake();
        }
        Poll::Ready(Ok(()))
    }
}

/// The guest-facing completion future: cancellation first, then upstream's value.
struct Reported {
    outcome: Arc<Mutex<Outcome>>,
    pipe: Arc<Mutex<Pipe>>,
}

impl<T: 'static> wasmtime::component::FutureProducer<T> for Reported {
    type Item = Result<(), ErrorCode>;

    fn poll_produce(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        _: StoreContextMut<'_, T>,
        finish: bool,
    ) -> Poll<wasmtime::Result<Option<Self::Item>>> {
        let mut outcome = self.outcome.lock().expect("outcome");
        if let Some(value) = outcome.value.take() {
            let cancelled = self.pipe.lock().expect("pipe").cancelled();
            return Poll::Ready(Ok(Some(if cancelled {
                Err(ErrorCode::Other(Some("call cancelled".into())))
            } else {
                value
            })));
        }
        if finish {
            return Poll::Ready(Ok(None));
        }
        outcome.reader = Some(context.waker().clone());
        Poll::Pending
    }
}

/// Pump one send: read `data` guest-side and write each chunk into the adapter writer upstream
/// consumes. On cancellation it closes the stream it waits on, joins the in-flight call, then
/// closes the other; it never starts a call after observing the cancellation. A finished input
/// ends upstream's input after the last chunk was written.
async fn pump_send<T: AdapterStore>(
    access: &Accessor<T>,
    exports: Exports,
    data: StreamReader<u8>,
    writer: u32,
    pipe: &PipeConsumer,
) -> wasmtime::Result<()> {
    let pipe = &pipe.0;
    let mut registered = None;
    let (reader,) = exports.open_bytes.call_concurrent(access, (data,)).await?;
    let abort = async || -> wasmtime::Result<()> {
        exports
            .close_bytes
            .call_concurrent(access, (reader,))
            .await?;
        exports
            .close_writer
            .call_concurrent(access, (writer,))
            .await
    };
    loop {
        if pipe.lock().expect("pipe").closed() {
            return abort().await;
        }
        let max = PIPE_BYTES as u32;
        let mut read = pin!(exports.read.call_concurrent(access, (reader, max)));
        let chunk = match select(read.as_mut(), pin!(closed(pipe))).await {
            Either::Left((read, _)) => read?.0,
            Either::Right(((), _)) => {
                let (close, read) =
                    join(exports.close_bytes.call_concurrent(access, (reader,)), read).await;
                close?;
                read?;
                return exports
                    .close_writer
                    .call_concurrent(access, (writer,))
                    .await;
            }
        };
        let Some(chunk) = chunk else {
            // The input ended (the adapter released its reader): end upstream's input.
            return exports
                .close_writer
                .call_concurrent(access, (writer,))
                .await;
        };
        // The command in which the guest produced this chunk owns the send while it is written.
        access.with(|mut store| own(store.data_mut().commands(), pipe, &mut registered));
        if pipe.lock().expect("pipe").closed() {
            return abort().await;
        }
        let mut write = pin!(exports.write.call_concurrent(access, (writer, chunk)));
        match select(write.as_mut(), pin!(closed(pipe))).await {
            Either::Left((written, _)) => {
                if !written?.0 {
                    // Upstream's reader is gone (the connection failed): stop reading the input.
                    return exports.close_bytes.call_concurrent(access, (reader,)).await;
                }
            }
            Either::Right(((), _)) => {
                let (close, write) = join(
                    exports.close_writer.call_concurrent(access, (writer,)),
                    write,
                )
                .await;
                close?;
                write?;
                return exports.close_bytes.call_concurrent(access, (reader,)).await;
            }
        }
    }
}

/// The compiled Store's exports, which `instantiate` created before the component.
fn compiled_exports<T: AdapterStore>(
    store: &mut StoreContextMut<'_, T>,
) -> wasmtime::Result<Exports> {
    store
        .data_mut()
        .adapter()
        .exports
        .ok_or_else(|| wasmtime::Error::msg("the private stream adapter was not instantiated"))
}

/// A compiled `tcp-socket.receive` stream: upstream's reader goes to a pump and the guest reads a
/// bounded pipe through a producer that its call's cancellation can end. The completion future is
/// upstream's, except that a cancellation-forced close reports `other("call cancelled")`.
pub(crate) fn receive<T: AdapterStore>(
    mut store: StoreContextMut<'_, T>,
    reader: StreamReader<u8>,
    done: Completion,
) -> wasmtime::Result<(StreamReader<u8>, Completion)> {
    let exports = compiled_exports(&mut store)?;
    let pipe: Arc<Mutex<Pipe>> = Arc::default();
    let done = completion(store.as_context_mut(), done, &pipe)?;
    spawn(
        store.as_context_mut(),
        exports,
        Source::Bytes(reader),
        PipeConsumer(Arc::clone(&pipe), None),
    )?;
    let data = StreamReader::new(
        store,
        GuestBytes(Guest {
            pipe,
            registered: None,
        }),
    )?;
    Ok((data, done))
}

/// The resource kind of compiled accepted sockets. Compiled Stores issue no handles: an accepted
/// socket is the exact upstream resource moved to the guest, and an unread one closes through
/// upstream's drop, so this kind is never looked up.
const UNISSUED: u32 = u32::MAX;

/// A compiled `tcp-socket.listen` stream: accepted sockets reach the guest as the exact upstream
/// resources; unread ones close through upstream.
pub(crate) fn listen<T: AdapterStore>(
    mut store: StoreContextMut<'_, T>,
    reader: StreamReader<Resource<TcpSocket>>,
) -> wasmtime::Result<StreamReader<Resource<TcpSocket>>> {
    let exports = compiled_exports(&mut store)?;
    let pipe: Arc<Mutex<Pipe>> = Arc::default();
    let released = Arc::clone(&store.data_mut().adapter().released);
    pipe.lock().expect("pipe").release_into(released);
    spawn(
        store.as_context_mut(),
        exports,
        Source::Sockets(reader, UNISSUED),
        PipeConsumer(Arc::clone(&pipe), None),
    )?;
    StreamReader::new(
        store,
        GuestSockets(Guest {
            pipe,
            registered: None,
        }),
    )
}

/// The adapter's exports in `store`, instantiating it (tests drive it directly).
#[cfg(test)]
pub(super) async fn test_exports(store: &mut wasmtime::Store<State>) -> wasmtime::Result<Exports> {
    exports(store).await
}

/// Take the newest listen stream bound by `tcp-socket.listen` before its pump starts, with its
/// resource kind, so a test can drive the adapter calls in a forced order (tests).
#[cfg(test)]
pub(super) fn take_unopened_sockets(
    store: &mut wasmtime::Store<State>,
) -> Option<(StreamReader<Resource<TcpSocket>>, u32)> {
    let unopened = &mut store.data_mut().standard.adapter.unopened;
    let index = unopened
        .iter()
        .rposition(|(source, _)| matches!(source, Source::Sockets(..)))?;
    match unopened.remove(index) {
        (Source::Sockets(reader, kind), _) => Some((reader, kind)),
        (Source::Bytes(_) | Source::Send(..), _) => unreachable!("matched a listen stream"),
    }
}

/// Send writers currently prepared for connected or accepted sockets (tests).
#[cfg(test)]
pub(super) fn prepared(state: &mut State) -> usize {
    state.standard.adapter.prepared.len()
}

/// Whether the socket at `rep` has a ready send writer (tests).
#[cfg(test)]
pub(super) fn ready(state: &mut State, rep: u32) -> bool {
    matches!(
        state.standard.adapter.prepared.get(&rep),
        Some(Prepared::Ready(..))
    )
}

/// Preparations that closed their writer because their entry was superseded or forgotten (tests).
#[cfg(test)]
pub(super) fn discarded(state: &mut State) -> usize {
    state.standard.adapter.discarded
}

/// A host byte stream that never produces (tests).
#[cfg(test)]
pub(super) fn idle_stream(store: &mut wasmtime::Store<State>) -> StreamReader<u8> {
    struct Idle;
    impl wasmtime::component::StreamProducer<State> for Idle {
        type Item = u8;
        type Buffer = Option<u8>;
        fn poll_produce<'a>(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            _: StoreContextMut<'a, State>,
            _: wasmtime::component::Destination<'a, u8, Option<u8>>,
            finish: bool,
        ) -> std::task::Poll<wasmtime::Result<wasmtime::component::StreamResult>> {
            if finish {
                std::task::Poll::Ready(Ok(wasmtime::component::StreamResult::Cancelled))
            } else {
                std::task::Poll::Pending
            }
        }
    }
    StreamReader::new(store.as_context_mut(), Idle).expect("stream")
}
