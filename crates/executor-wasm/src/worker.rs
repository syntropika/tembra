//! Keep Wasmtime on an owned worker; only scope-bound requests cross to the native host.
//!
//! One session owns one Store per Orb revision. A source revision runs the interpreter core
//! module behind the private fixed-buffer ABI in `abi.rs`. Its preview 1 imports carry no
//! filesystem or network authority; generated WASI calls use its separately granted context.
//! A compiled revision runs its component on the same owner and
//! speaks the same private host protocol (see `component::worker`).
use crate::protocol::GuestCommand;
use crate::{Engine, abi::Operation, failure};
use async_channel::{Receiver, Sender};
use futures::{FutureExt, StreamExt, stream::FuturesUnordered};
use serde_json::Value;
use std::sync::Arc;
use std::{
    cell::Cell,
    rc::Rc,
    sync::{Mutex, mpsc},
    thread,
};
use tembra::Result;
use tokio::sync::oneshot;

pub(crate) mod diagnostics;

/// Blocking threads of one Store's runtime, used by upstream WASI filesystem calls.
const STORE_BLOCKING_THREADS: usize = 2;
mod runtime;
mod standard;
mod steps;
pub(crate) mod wasi;

pub(crate) use runtime::check;
pub(crate) use standard::{Pipe, PipeConsumer, StreamAdapter, adapter as stream_adapter};

pub(crate) enum Event {
    Host {
        packet: String,
        asynchronous: bool,
        reply: oneshot::Sender<std::result::Result<String, String>>,
    },
    Done(std::result::Result<String, String>),
    /// Incoming HTTP carrier events of a platform exchange (`crate::ingress::IngressPort`).
    Ingress(crate::ingress::IngressEvent),
    /// A guest's asynchronous import whose whole result is a `stream<u8>`: a compiled guest's
    /// stream import, or a source guest's `STREAM_IMPORT`. The Store created `pipe` for the
    /// guest's reader; the host fills it from the delivered stream end. It is never serialized,
    /// and no number in `packet` names it.
    Stream {
        packet: String,
        reply: oneshot::Sender<std::result::Result<String, String>>,
        pipe: Arc<Mutex<Pipe>>,
    },
}

/// How a guest's host request reached the host. A request on the stream carrier may only call
/// an import whose whole result is a `stream<u8>`, and such an import is called only on it; the
/// host checks this before it issues the call.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Carried {
    /// A synchronous request: the guest stack waits for it.
    Sync,
    /// An asynchronous request whose answer is data, or resources the reply delivers.
    Async,
    /// An asynchronous import call answered with a stream end, pumped into the Store's pipe.
    Stream,
}

/// The dependency call one host request issued, if any. When the callback stops waiting for
/// that request, its answer cancels exactly this call and keeps owning it until it has
/// physically ended.
pub(crate) type Issued = Cell<Option<tembra::IssuedCall>>;

/// Cancels the dependency call of a request whose answer is dropped before it settled (its
/// callback's driver went away): never silently detached, at least signalled.
struct Abandon<'a>(&'a Issued);

impl Drop for Abandon<'_> {
    fn drop(&mut self) {
        if let Some(call) = self.0.take() {
            call.cancel();
        }
    }
}

/// Whether a callback still joins the dependency calls its abandoned requests issued. It stops
/// when the caller's cancellation revoked the callback or its Store stopped: those calls are then
/// only signalled and stay counted by the kernel, as for a root cancellation. A frame value:
/// waiters allocate only when a request was abandoned.
#[derive(Default)]
struct Release {
    released: Cell<bool>,
    waiters: std::cell::RefCell<Vec<std::task::Waker>>,
}

impl Release {
    fn release(&self) {
        self.released.set(true);
        for waiter in self.waiters.take() {
            waiter.wake();
        }
    }

    async fn released(&self) {
        std::future::poll_fn(|context| {
            if self.released.get() {
                return std::task::Poll::Ready(());
            }
            let mut waiters = self.waiters.borrow_mut();
            if !waiters
                .iter()
                .any(|waiter| waiter.will_wake(context.waker()))
            {
                waiters.push(context.waker().clone());
            }
            std::task::Poll::Pending
        })
        .await
    }
}

/// Stream imports one callback may have outstanding (requested, or delivered and being read).
const STREAM_IMPORTS: usize = 16;

/// One answer to a guest's host request.
///
/// Capabilities installed for the answer stay owned by its `delivery` until the worker has
/// actually received the reply. Only then are they committed; otherwise dropping the delivery
/// rolls them back and closes them.
pub(crate) struct Reply {
    pub(crate) value: Value,
    pub(crate) delivery: Option<crate::bindings::Delivery>,
    /// The delivered end of a stream import's result; it never leaves the host thread.
    pub(crate) stream: Option<tembra::portable::ByteStream>,
}

impl From<Value> for Reply {
    fn from(value: Value) -> Self {
        Self {
            value,
            delivery: None,
            stream: None,
        }
    }
}

/// One host-selected operation and the channel that carries its scoped requests and completion.
pub(crate) struct Command {
    pub(crate) operation: Operation,
    pub(crate) packet: String,
    pub(crate) events: Sender<Event>,
    /// The typed request and head channel of an incoming HTTP call; never serialized.
    pub(crate) ingress: Option<crate::ingress::IngressCall>,
    /// The host-created end of a compiled call's byte-stream result; never serialized.
    pub(crate) stream: Option<StreamCall>,
}

/// Typed values a callback carries beside its packet. They never cross as JSON, so a guest
/// cannot name or forge them.
#[derive(Default)]
pub(crate) enum Carrier {
    #[default]
    None,
    Ingress(Box<crate::ingress::IngressCall>),
    Stream(StreamCall),
}

/// A compiled export's byte-stream result, created by the host before the call.
///
/// The Store pipes the guest's stream into `pipe` exactly once and then signals `head`; the
/// call keeps running, under its admission, until its task group and the pipe have ended.
pub(crate) struct StreamCall {
    pub(crate) pipe: Arc<Mutex<Pipe>>,
    pub(crate) head: futures::channel::oneshot::Sender<()>,
}

/// Terminal completions that must wait until the Store and its runtime have stopped.
pub(crate) type Terminal = Vec<(Sender<Event>, String)>;

struct Owner {
    commands: Option<mpsc::Sender<Command>>,
    thread: Option<thread::JoinHandle<()>>,
    busy: Cell<bool>,
    packet_bytes: usize,
    admission: Arc<futures::lock::Mutex<()>>,
    poison: Arc<Mutex<Option<String>>>,
    heap_bytes: usize,
    diagnostics: diagnostics::Diagnostics,
    admission_ms: u64,
    io: crate::io::SourceIo,
}

impl Owner {
    /// Release buffered replies and refuse future calls; the first recorded reason wins.
    /// The abandoned callback's epoch ends first, so its pending standard work is revoked.
    fn abandon(&self, events: &Receiver<Event>, reason: &str) {
        self.io.scope().end_callback();
        close_replies(events);
        self.poison
            .lock()
            .expect("poison state")
            .get_or_insert_with(|| reason.into());
    }
}

impl Drop for Owner {
    /// Close the command stream and join only this interpreter's worker.
    fn drop(&mut self) {
        self.io.scope().end_callback();
        self.commands.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The native facade remains thread-affine; no callback or kernel handle enters the Store.
#[derive(Clone)]
pub(crate) struct Session(Rc<Owner>);

impl Session {
    /// Share the callback epoch so a foreign event receiver can end the callback.
    pub(crate) fn scope(&self) -> crate::io::CallbackScope {
        self.0.io.scope().clone()
    }

    /// Start one source Store with this installation's checked standard context.
    pub(crate) fn start(engine: Engine, orb: String, io: crate::io::SourceIo) -> Result<Self> {
        let worker = engine.clone();
        let standard = io
            .take_standard()
            .ok_or_else(|| failure("source installation context was already used"))?;
        Self::spawn(&engine, orb, io, move |commands, poison, diagnostics| {
            runtime::run(worker, commands, poison, diagnostics, standard)
        })
    }

    /// Run one Store loop on a dedicated thread behind the shared admission and poison owner.
    ///
    /// The loop receives only owned commands; its terminal completions are published after its
    /// runtime, including blocking WASI work, has stopped.
    pub(crate) fn spawn<F, Run>(
        engine: &Engine,
        orb: String,
        io: crate::io::SourceIo,
        run: F,
    ) -> Result<Self>
    where
        F: FnOnce(
                mpsc::Receiver<Command>,
                Arc<Mutex<Option<String>>>,
                diagnostics::Diagnostics,
            ) -> Run
            + Send
            + 'static,
        Run: std::future::Future<Output = Terminal>,
    {
        let (send, receive) = mpsc::channel();
        let packet_bytes = engine.limits.packet_bytes;
        let heap_bytes = engine.limits.memory_bytes / 2;
        let diagnostics = diagnostics::Diagnostics {
            orb,
            sink: engine.logger.clone(),
        };
        let worker_diagnostics = diagnostics.clone();
        let terminal_diagnostics = diagnostics.clone();
        let admission_ms = engine.limits.timeout_ms / 2;
        let poison = Arc::new(Mutex::new(None));
        let worker_poison = poison.clone();
        let instances = engine.active_instances.clone();
        let thread = thread::Builder::new()
            .name("tembra-wasm".into())
            .spawn(move || {
                instances.fetch_add(1, std::sync::atomic::Ordering::Release);
                let count = Count(instances);
                // Compiled upstream work has a small fallback pool. Qualified source file calls
                // run inline under the process-wide active budget in io::file_turn.
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .max_blocking_threads(STORE_BLOCKING_THREADS)
                    .enable_all()
                    .build();
                if let Ok(runtime) = runtime {
                    let terminal =
                        runtime.block_on(run(receive, worker_poison.clone(), worker_diagnostics));
                    publish_after_shutdown(
                        runtime,
                        count,
                        &worker_poison,
                        &terminal_diagnostics,
                        terminal,
                    );
                }
            })
            .map_err(failure)?;
        Ok(Self(Rc::new(Owner {
            commands: Some(send),
            thread: Some(thread),
            busy: Cell::new(false),
            packet_bytes,
            admission: Arc::new(futures::lock::Mutex::new(())),
            poison,
            heap_bytes,
            diagnostics,
            admission_ms,
            io,
        })))
    }

    /// Reserve the serialized interpreter before the host enters a synchronous factory.
    pub(crate) async fn reserve_factory(&self) -> Result<futures::lock::OwnedMutexGuard<()>> {
        let admission = std::pin::pin!(self.0.admission.clone().lock_owned());
        let deadline = std::pin::pin!(futures_timer::Delay::new(std::time::Duration::from_millis(
            self.0.admission_ms
        )));
        match futures::future::select(admission, deadline).await {
            futures::future::Either::Left((permit, _)) => Ok(permit),
            futures::future::Either::Right(_) => Err(tembra::Error::new(
                tembra::core::Code::Unavailable,
                "wasm_executor_admission",
                "factory admission exceeded its wait limit; possible causal reentry",
            )),
        }
    }

    /// Load source without issuing any host authority to module initialization.
    pub(crate) fn load(&self, source: &str, contract: Value) -> Result<Value> {
        let packet = crate::protocol::Load {
            source: source.into(),
            heap_bytes: self.0.heap_bytes,
            contract,
            instruction_ticks: crate::protocol::DEFAULT_INSTRUCTION_TICKS,
            builtins: self.0.io.builtins(),
        };
        self.blocking(
            Operation::Load,
            serde_json::to_string(&packet).map_err(failure)?,
            |packet, _| self.log(packet),
        )
    }

    /// Accept diagnostics independently of execution authority and attribute them to this revision.
    pub(crate) fn log(&self, packet: &str) -> Result<Value> {
        self.0
            .diagnostics
            .accept(packet)
            .unwrap_or_else(|| Err(failure("module initialization has no host capability")))
    }

    /// Queue pure guest bookkeeping after the native scope's last owner is released.
    pub(crate) fn release_environment(&self, environment: u64) {
        self.cleanup(GuestCommand::Release { environment }, None);
    }

    /// Release an environment whose revision holds imported resources; see [`Self::cleanup`].
    pub(crate) fn release_environment_with(&self, environment: u64, release: &mut dyn FnMut(u32)) {
        self.cleanup(GuestCommand::Release { environment }, Some(release));
    }

    /// Destroy one exported resource whose last owner was dropped; see [`Self::cleanup`].
    pub(crate) fn destroy(
        &self,
        environment: u64,
        instance: u32,
        release: Option<&mut dyn FnMut(u32)>,
    ) {
        self.cleanup(
            GuestCommand::Destroy {
                environment,
                instance,
            },
            release,
        );
    }

    /// Run a host-initiated cleanup: an environment release or a resource destruction.
    ///
    /// It needs no admission and no caller. When the revision is idle and holds imported
    /// resources (`release` is given), it runs now on this thread, and every imported slot its
    /// destructors drop is passed to `release`. That only removes host table entries, and grants
    /// the destructors nothing. Otherwise the command is queued behind the running callback; the
    /// slots its destructors drop then wait for this revision's next call or its root's release.
    /// A failure is reported, never passed off as a clean release.
    fn cleanup(&self, packet: GuestCommand<'_>, release: Option<&mut dyn FnMut(u32)>) {
        let packet = serde_json::to_string(&packet).expect("serializable cleanup");
        let poisoned = self.0.poison.lock().expect("poison state").is_some();
        if let Some(release) = release.filter(|_| !self.0.busy.get() && !poisoned) {
            let outcome = self.blocking(Operation::Start, packet, |packet, _| {
                match crate::protocol::Request::decode(packet) {
                    Ok(crate::protocol::Request::ReferenceRelease { id, .. }) => {
                        release(id);
                        Ok(Value::Null)
                    }
                    _ => Err(failure("a cleanup performs no other host operation")),
                }
            });
            if let Err(error) = outcome {
                let cause = self.0.poison.lock().expect("poison state").clone();
                let _ = self.0.diagnostics.emit(
                    crate::protocol::LogLevel::Error,
                    match cause {
                        Some(cause) => format!("revision quarantined: {cause}"),
                        None => format!("resource cleanup failed: {error}"),
                    },
                );
            }
            return;
        }
        let (events, receive) = async_channel::bounded(1);
        drop(receive);
        let command = Command {
            operation: Operation::Start,
            packet,
            events,
            ingress: None,
            stream: None,
        };
        if let Some(sender) = &self.0.commands {
            let _ = sender.send(command);
        }
    }

    /// Submit one callback, refusing reentry before it could deadlock on this Store.
    pub(crate) fn begin(&self, operation: Operation, packet: String) -> Result<Receiver<Event>> {
        self.begin_with(operation, packet, Carrier::None)
    }

    /// Submit an incoming HTTP callback for a platform port. The typed request travels beside
    /// the command; the returned sender lets its carrier publish events on the same stream.
    pub(crate) fn begin_ingress(
        &self,
        packet: String,
        head: crate::ingress::IngressRequestHead,
    ) -> Result<(Receiver<Event>, crate::ingress::IngressPort)> {
        let (sender, events) = async_channel::bounded(128);
        let (call, port) = crate::ingress::begin_carrier(head, sender.clone())?;
        let receive = self.submit(
            Operation::Start,
            packet,
            Carrier::Ingress(Box::new(call)),
            (sender, events),
        )?;
        Ok((receive, port))
    }

    /// Submit one callback with its typed carrier, if any.
    fn begin_with(
        &self,
        operation: Operation,
        packet: String,
        carrier: Carrier,
    ) -> Result<Receiver<Event>> {
        self.submit(operation, packet, carrier, async_channel::bounded(128))
    }

    /// Queue one callback on its own event channel, refusing reentry before it could deadlock.
    fn submit(
        &self,
        operation: Operation,
        packet: String,
        carrier: Carrier,
        (send, receive): (Sender<Event>, Receiver<Event>),
    ) -> Result<Receiver<Event>> {
        if let Some(poison) = self.0.poison.lock().expect("poison state").as_ref() {
            return Err(tembra::Error::new(
                tembra::core::Code::Unavailable,
                "wasm_executor_quarantined",
                poison,
            ));
        }
        if packet.len() > self.0.packet_bytes {
            return Err(failure("interpreter packet exceeds its configured limit"));
        }
        if self.0.busy.replace(true) {
            return Err(failure(
                "concurrent or reentrant calls to this interpreter are not supported",
            ));
        }
        let (ingress, stream) = match carrier {
            Carrier::None => (None, None),
            Carrier::Ingress(call) => (Some(*call), None),
            Carrier::Stream(call) => (None, Some(call)),
        };
        let command = Command {
            operation,
            packet,
            events: send,
            ingress,
            stream,
        };
        // Transport streams opened by this callback belong to its epoch until it completes.
        self.0.io.scope().begin_callback();
        if self
            .0
            .commands
            .as_ref()
            .expect("live owner")
            .send(command)
            .is_err()
        {
            self.0.io.scope().end_callback();
            self.0.busy.set(false);
            return Err(failure("interpreter worker has stopped"));
        }
        Ok(receive)
    }

    /// Reopen a raw platform exchange only after its receiver observed physical completion.
    pub(crate) fn finish_exchange(&self) {
        self.0.busy.set(false);
    }

    /// Close buffered reply senders and refuse future calls when a platform driver is abandoned.
    pub(crate) fn abandon_exchange(&self, events: &Receiver<Event>) {
        self.0.abandon(
            events,
            "platform callback driver ended before physical settlement",
        );
    }

    /// Execute a synchronous declaration while servicing only immediate scope operations.
    pub(crate) fn blocking(
        &self,
        operation: Operation,
        packet: String,
        mut host: impl FnMut(&str, bool) -> Result<Value>,
    ) -> Result<Value> {
        let events = self.begin(operation, packet)?;
        let mut guard = Busy {
            owner: &self.0,
            events: &events,
            completed: false,
        };
        loop {
            match events.recv_blocking().map_err(failure)? {
                // Carrier events exist only on platform ingress exchanges.
                Event::Ingress(_) => {}
                Event::Done(result) => {
                    self.0.io.scope().end_callback();
                    guard.completed = true;
                    return decode(result);
                }
                Event::Host {
                    packet,
                    asynchronous,
                    reply,
                } => {
                    let result = if asynchronous {
                        Err(failure(
                            "factories cannot perform asynchronous host operations",
                        ))
                    } else {
                        host(&packet, false)
                    };
                    let _ = reply.send(encode(
                        result.map(|value| value.to_string()),
                        self.0.packet_bytes,
                    ));
                }
                Event::Stream { reply, .. } => {
                    let _ = reply.send(Err("declarations cannot import streams".into()));
                }
            }
        }
    }

    /// Wait for the serialized interpreter; a cancelled caller leaves the queue before guest code.
    pub(crate) async fn admit(
        &self,
        cancellation: Option<&tembra::Cancellation>,
    ) -> Result<futures::lock::OwnedMutexGuard<()>> {
        let admission = std::pin::pin!(self.0.admission.clone().lock_owned());
        let Some(cancellation) = cancellation else {
            return Ok(admission.await);
        };
        let cancelled = std::pin::pin!(cancellation.cancelled());
        match futures::future::select(admission, cancelled).await {
            futures::future::Either::Left((permit, _)) => Ok(permit),
            // Dropping the lock future leaves the queue; no Store or callback was claimed.
            futures::future::Either::Right(_) => {
                cancellation.check()?;
                Err(failure("cancellation completed without a request"))
            }
        }
    }

    /// Keep the callback physically owned until guest completion and requested host work settle.
    /// The admission permit keeps this Store serialized until the callback is physically settled.
    ///
    /// An incoming HTTP call carries its typed payload here; it publishes its response head on
    /// its own channel and completes like any callback, only after physical settlement.
    pub(crate) async fn dispatch(
        &self,
        _admission: futures::lock::OwnedMutexGuard<()>,
        cancellation: Option<&tembra::Cancellation>,
        packet: impl serde::Serialize,
        carrier: Carrier,
        host: impl AsyncFn(&str, Carried, &Issued) -> Result<Reply>,
    ) -> Result<Value> {
        let events = self.begin_with(
            Operation::Start,
            serde_json::to_string(&packet).map_err(failure)?,
            carrier,
        )?;
        let mut guard = Busy {
            owner: &self.0,
            events: &events,
            completed: false,
        };
        // Outstanding stream imports of this callback; a plain counter on this frame, so data
        // requests pay nothing for it. Declared first: the pending answers borrow it, and
        // whether they still join the dependency calls of abandoned requests.
        let streams = Cell::new(0_usize);
        let release = Release::default();
        let mut pending = FuturesUnordered::new();
        let mut revoked = false;
        loop {
            let next = async {
                loop {
                    if pending.is_empty() {
                        return events.recv().await;
                    }
                    let event = std::pin::pin!(events.recv());
                    let completed = std::pin::pin!(pending.next());
                    match futures::future::select(completed, event).await {
                        futures::future::Either::Left(_) => continue,
                        futures::future::Either::Right((event, _)) => return event,
                    }
                }
            };
            let event = if let Some(cancellation) = cancellation.filter(|_| !revoked) {
                let next = std::pin::pin!(next);
                let cancelled = std::pin::pin!(cancellation.cancelled());
                match futures::future::select(next, cancelled).await {
                    futures::future::Either::Left((event, _)) => event,
                    futures::future::Either::Right(_) => {
                        self.0.io.scope().end_callback();
                        revoked = true;
                        release.release();
                        continue;
                    }
                }
            } else {
                next.await
            };
            let event = event.map_err(|error| {
                // The revision stopped before this callback completed; report why it stopped.
                match self.0.poison.lock().expect("poison state").as_ref() {
                    Some(cause) => tembra::Error::new(
                        tembra::core::Code::Unavailable,
                        "wasm_executor_quarantined",
                        cause,
                    ),
                    None => failure(error),
                }
            });
            match event? {
                // The native facade reads its response through the call's `Exchange`; carrier
                // events exist only on platform ingress exchanges.
                Event::Ingress(_) => {}
                Event::Done(result) => {
                    // End this callback's epoch before waiting, so abandoned requests settle now;
                    // their completion is still awaited. A stopped Store only signals them.
                    self.0.io.scope().end_callback();
                    if self.0.poison.lock().expect("poison state").is_some() {
                        release.release();
                    }
                    while pending.next().await.is_some() {}
                    guard.completed = true;
                    return decode(result);
                }
                Event::Host {
                    packet,
                    asynchronous,
                    reply,
                } => pending.push(self.answer(&host, packet, asynchronous, reply, None, &release)),
                Event::Stream {
                    packet,
                    reply,
                    pipe,
                } => {
                    if streams.get() >= STREAM_IMPORTS {
                        let _ = reply.send(Err(format!(
                            "a callback may have at most {STREAM_IMPORTS} stream imports outstanding"
                        )));
                        continue;
                    }
                    let slot = StreamSlot::take(&streams);
                    pending.push(self.answer(
                        &host,
                        packet,
                        true,
                        reply,
                        Some((pipe, slot)),
                        &release,
                    ));
                }
            }
        }
    }
}

impl Session {
    /// Answer one host request of a callback. Capabilities installed for the answer are
    /// committed only once the worker received it; anything else rolls back. A stream import's
    /// delivered end is then pumped into its pipe, within this callback: the pump ends at the
    /// stream's end, at a failure, when the guest closes its reader, or when the callback's
    /// epoch ends.
    ///
    /// If the callback stops waiting for the answer while a dependency call it issued is still
    /// running, that call is cancelled at once: it and the calls it issued only, never its root
    /// or another call. While the callback settles normally the call also stays owned by this
    /// answer until it has physically ended, so the callback, and a stream it published,
    /// settles only after it; a callee that ignores the cancellation keeps it unsettled, and
    /// counted, as long as it runs. If the caller's cancellation revoked the callback or its
    /// Store stopped, the call is only signalled, and stays counted by the kernel, as for a root
    /// cancellation.
    fn answer<'a>(
        &'a self,
        host: &'a impl AsyncFn(&str, Carried, &Issued) -> Result<Reply>,
        packet: String,
        asynchronous: bool,
        mut reply: oneshot::Sender<std::result::Result<String, String>>,
        stream: Option<(Arc<Mutex<Pipe>>, StreamSlot<'a>)>,
        release: &'a Release,
    ) -> futures::future::LocalBoxFuture<'a, ()> {
        async move {
            let issued = Issued::default();
            let _abandon = Abandon(&issued);
            let carried = match (&stream, asynchronous) {
                (Some(_), _) => Carried::Stream,
                (None, true) => Carried::Async,
                (None, false) => Carried::Sync,
            };
            let result = {
                let operation = std::pin::pin!(host(&packet, carried, &issued));
                let closed = std::pin::pin!(reply.closed());
                match futures::future::select(operation, closed).await {
                    futures::future::Either::Left((result, _)) => Some(result),
                    futures::future::Either::Right(_) => None,
                }
            };
            let Some(result) = result else {
                if let Some(call) = issued.take() {
                    call.cancel();
                    let settled = std::pin::pin!(call.settled());
                    let released = std::pin::pin!(release.released());
                    futures::future::select(settled, released).await;
                }
                return;
            };
            // Answered: the dependency call completed for this request; nothing to cancel.
            issued.take();
            let (text, delivery, end) = match result {
                Ok(answer) => (Ok(answer.value.to_string()), answer.delivery, answer.stream),
                Err(error) => (Err(error), None, None),
            };
            // A stream end is answered only to a stream import, and a stream import only with
            // an end; a mismatch refuses, which drops (and so closes) any end.
            let text = match (&end, &stream, text) {
                (Some(_), Some(_), text) | (None, None, text) | (_, _, text @ Err(_)) => text,
                _ => Err(failure("the import's stream result differs from its call")),
            };
            let text = encode(text, self.0.packet_bytes);
            let accepted = text.is_ok();
            if reply.send(text).is_err() || !accepted {
                return;
            }
            if let Some(delivery) = delivery {
                delivery.commit();
            }
            if let (Some(end), Some((pipe, _slot))) = (end, stream) {
                crate::bindings::pump(end, pipe, self.0.io.scope().cancelled()).await;
            }
        }
        .boxed_local()
    }
}

/// One outstanding stream import of a callback, released when its answer and pump end.
struct StreamSlot<'a>(&'a Cell<usize>);

impl<'a> StreamSlot<'a> {
    fn take(count: &'a Cell<usize>) -> Self {
        count.set(count.get() + 1);
        Self(count)
    }
}

impl Drop for StreamSlot<'_> {
    fn drop(&mut self) {
        self.0.set(self.0.get() - 1);
    }
}

struct Busy<'a> {
    owner: &'a Owner,
    events: &'a Receiver<Event>,
    completed: bool,
}
impl Drop for Busy<'_> {
    /// Reopen admission only after physical completion; poison an abandoned driver.
    fn drop(&mut self) {
        if self.completed {
            self.owner.busy.set(false);
        } else {
            self.owner.abandon(
                self.events,
                "interpreter callback driver ended before physical settlement",
            );
        }
    }
}

/// Runtime destruction joins WASI blocking work before a quarantine becomes observable.
///
/// The live-Store metric retires first, then the first terminal diagnostic becomes the
/// quarantine reason, then each terminal completion is published.
fn publish_after_shutdown(
    runtime: tokio::runtime::Runtime,
    count: Count,
    poison: &Mutex<Option<String>>,
    diagnostics: &diagnostics::Diagnostics,
    terminal: Terminal,
) {
    drop(runtime);
    drop(count);
    if let Some((_, error)) = terminal.first() {
        poison
            .lock()
            .expect("poison state")
            .get_or_insert_with(|| error.clone());
    }
    for (events, error) in terminal {
        // A command nobody awaits, such as a resource destruction or an environment release,
        // has no caller to receive its failure; report it rather than letting it pass silently.
        if events
            .send_blocking(Event::Done(Err(error.clone())))
            .is_err()
        {
            let _ = diagnostics.emit(
                crate::protocol::LogLevel::Error,
                format!("revision quarantined: {error}"),
            );
        }
    }
}

/// Count one live Store until its thread has dropped the Store and its runtime.
struct Count(Arc<std::sync::atomic::AtomicUsize>);
impl Drop for Count {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::Release);
    }
}

/// Parse guest output only after preserving boundary diagnostics.
fn decode(result: std::result::Result<String, String>) -> Result<Value> {
    serde_json::from_str(&result.map_err(failure)?).map_err(failure)
}

/// Close an event stream and drop its buffered reply senders so no request outlives its driver.
pub(crate) fn close_replies(events: &Receiver<Event>) {
    events.close();
    while let Ok(event) = events.try_recv() {
        drop(event);
    }
}

/// Bound one encoded host result without transferring native handles or callbacks.
pub(crate) fn encode(result: Result<String>, limit: usize) -> std::result::Result<String, String> {
    let value = result.map_err(|error| error.to_string())?;
    if value.len() > limit {
        return Err("host result exceeds the configured packet limit".into());
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Contract, Limits};
    use std::time::{Duration, Instant};

    #[test]
    fn callback_release_wakes_each_waiter_once_after_repeated_polls() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Default)]
        struct Wakes(AtomicUsize);
        impl futures::task::ArcWake for Wakes {
            fn wake_by_ref(this: &Arc<Self>) {
                this.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        let release = Release::default();
        let first = Arc::new(Wakes::default());
        let second = Arc::new(Wakes::default());
        let first_waker = futures::task::waker(Arc::clone(&first));
        let second_waker = futures::task::waker(Arc::clone(&second));
        let mut first_context = std::task::Context::from_waker(&first_waker);
        let mut second_context = std::task::Context::from_waker(&second_waker);
        let mut first_wait = std::pin::pin!(release.released());
        let mut second_wait = std::pin::pin!(release.released());
        for _ in 0..1024 {
            assert!(first_wait.as_mut().poll(&mut first_context).is_pending());
            assert!(second_wait.as_mut().poll(&mut second_context).is_pending());
        }
        release.release();
        assert_eq!(first.0.load(Ordering::Relaxed), 1);
        assert_eq!(second.0.load(Ordering::Relaxed), 1);
        assert!(first_wait.as_mut().poll(&mut first_context).is_ready());
        assert!(second_wait.as_mut().poll(&mut second_context).is_ready());
        release.release();
        assert_eq!(first.0.load(Ordering::Relaxed), 1);
        assert_eq!(second.0.load(Ordering::Relaxed), 1);
    }

    /// A detached WASI blocking operation must finish before quarantine is published.
    #[test]
    fn quarantine_waits_for_blocking_runtime_work() {
        let (started, ready) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let (events, receive) = async_channel::bounded(1);
        let thread = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.spawn_blocking(move || {
                let _ = started.send(());
                let _ = blocked.recv();
            });
            let instances = Arc::new(std::sync::atomic::AtomicUsize::new(1));
            let poison = Mutex::new(None);
            publish_after_shutdown(
                runtime,
                Count(instances),
                &poison,
                &diagnostics::Diagnostics {
                    orb: "test".into(),
                    sink: None,
                },
                vec![(events, "quarantined".into())],
            );
        });
        ready.recv().unwrap();
        let (published, observed) = mpsc::channel();
        let observer = thread::spawn(move || {
            let _ = published.send(receive.recv_blocking());
        });
        let premature = observed.recv_timeout(Duration::from_millis(50));
        release.send(()).unwrap();
        thread.join().unwrap();
        assert!(
            premature.is_err(),
            "quarantine preceded physical completion"
        );
        assert!(
            matches!(observed.recv().unwrap(), Ok(Event::Done(Err(error))) if error == "quarantined")
        );
        observer.join().unwrap();
    }

    /// Abandon an actual buffered synchronous import and prove Store destruction cannot hang.
    #[test]
    #[ignore = "requires TEMBRA_WASM_ENGINE"]
    fn abandoned_buffered_sync_import_releases_worker() {
        let bytes =
            std::fs::read(std::env::var_os("TEMBRA_WASM_ENGINE").expect("TEMBRA_WASM_ENGINE"))
                .unwrap();
        let engine = Engine::new(Limits {
            timeout_ms: 500,
            ..Limits::default()
        })
        .unwrap()
        .interpreter(&bytes)
        .unwrap();
        let session = Session::start(
            engine.clone(),
            "abandoned".into(),
            crate::io::SourceIo::new(
                &engine.providers,
                &[],
                &crate::Grants::default(),
                &crate::Cli::default(),
            )
            .unwrap(),
        )
        .unwrap();
        let contract = Contract::from_source("checks.wit","package example:abandon; interface checks { run: func() -> u32; } world test { export checks; }",None).unwrap();
        session.load(r#"globalThis.__tembraOrb={services:{checks:{create(){__hostSync('{"operation":"state.read"}');return {}},methods:{run(){return 1}}}}};"#,contract.describe()).unwrap();
        let events = session
            .begin(
                Operation::Declare,
                serde_json::json!({"kind":"create","environment":1,"service":"checks"}).to_string(),
            )
            .unwrap();
        let guard = Busy {
            owner: &session.0,
            events: &events,
            completed: false,
        };
        let started = Instant::now();
        while events.is_empty() && started.elapsed() < Duration::from_millis(400) {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(
            !events.is_empty(),
            "the real synchronous import was not buffered"
        );
        drop(guard);
        drop(events);
        let started = Instant::now();
        drop(session);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(engine.instances(), 0);
    }

    /// Load the real interpreter into a bare Store for transport-level checks.
    fn transport(memory_bytes: usize) -> (Engine, tokio::runtime::Runtime) {
        let bytes =
            std::fs::read(std::env::var_os("TEMBRA_WASM_ENGINE").expect("TEMBRA_WASM_ENGINE"))
                .unwrap();
        let engine = Engine::new(Limits {
            memory_bytes,
            ..Limits::default()
        })
        .unwrap()
        .interpreter(&bytes)
        .unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        (engine, runtime)
    }

    /// Snapshot capture requires a loaded idle heap and returns its bytes through the ABI.
    #[test]
    #[ignore = "requires TEMBRA_WASM_ENGINE"]
    fn idle_heap_snapshot_capture() {
        let (engine, runtime) = transport(Limits::default().memory_bytes);
        let contract = Contract::from_source("checks.wit", "package example:capture; interface checks { run: func() -> u32; } world test { export checks; }", None).unwrap();
        runtime.block_on(async {
            let diagnostics = diagnostics::Diagnostics {
                orb: "capture".into(),
                sink: None,
            };
            let (mut store, driver) = runtime::instantiate(
                &engine,
                diagnostics,
                crate::io::StandardContext {
                    clocks: None,
                    resolve: false,
                    wasi: wasmtime_wasi::WasiCtxBuilder::new(),
                    allowed: Default::default(),
                    scope: {
                        let scope = crate::io::CallbackScope::default();
                        scope.begin_callback();
                        scope
                    },
                    grants: Default::default(),
                },
                Default::default(),
            )
            .await
            .unwrap();
            assert!(driver.capture(&mut store).await.unwrap().is_err());
            let load = serde_json::json!({
                "source": "globalThis.__tembraOrb={services:{checks:{methods:{run(){return 1}}}}};",
                "heap_bytes": 32 << 20,
                "contract": contract.describe(),
            });
            let outcome = driver
                .call(Operation::Load, load.to_string(), &mut store)
                .await
                .unwrap();
            assert!(outcome.value.is_ok() && !outcome.interrupted);
            let snapshot = driver.capture(&mut store).await.unwrap().unwrap();
            assert!(snapshot.len() > 1024, "snapshot has heap contents");
        });
    }

    /// The host-supplied memory maximum bounds total linear memory independently of the VM heap.
    #[test]
    #[ignore = "requires TEMBRA_WASM_ENGINE"]
    fn imported_memory_maximum_bounds_linear_memory_beyond_the_heap_limit() {
        let memory_bytes = 8 << 20;
        let (engine, runtime) = transport(memory_bytes);
        let contract = Contract::from_source("checks.wit", "package example:memory; interface checks { run: func() -> u32; } world test { export checks; }", None).unwrap();
        runtime.block_on(async {
            let diagnostics = diagnostics::Diagnostics {
                orb: "memory".into(),
                sink: None,
            };
            let (mut store, driver) = runtime::instantiate(
                &engine,
                diagnostics,
                crate::io::StandardContext {
                    clocks: None,
                    resolve: false,
                    wasi: wasmtime_wasi::WasiCtxBuilder::new(),
                    allowed: Default::default(),
                    scope: { let scope = crate::io::CallbackScope::default(); scope.begin_callback(); scope },
                    grants: Default::default(),
                },
                Default::default()
            )
            .await
            .unwrap();
            // The VM heap limit is deliberately larger than the linear-memory maximum.
            let load = serde_json::json!({
                "source": "const chunks=[];for(let i=0;i<64;i++)chunks.push(new Uint8Array(1<<20));globalThis.__tembraOrb={services:{checks:{methods:{run(){return 1}}}}};",
                "heap_bytes": 256 << 20,
                "contract": contract.describe(),
            });
            let outcome = driver.call(Operation::Load, load.to_string(), &mut store).await;
            let failed = outcome.map_or(true, |outcome| outcome.value.is_err() && outcome.interrupted);
            assert!(failed, "allocation beyond the memory maximum must fail the instance");
            let memory = store.data().memory.unwrap();
            assert!(memory.data_size(&store) <= memory_bytes);
        });
    }

    /// Start a session whose fuel and wall time leave the guest tick bound as the first limit.
    fn tick_bounded_session(orb: &str) -> (Engine, Session) {
        let bytes =
            std::fs::read(std::env::var_os("TEMBRA_WASM_ENGINE").expect("TEMBRA_WASM_ENGINE"))
                .unwrap();
        let engine = Engine::new(Limits {
            fuel: 10_000_000_000,
            timeout_ms: 5000,
            ..Limits::default()
        })
        .unwrap()
        .interpreter(&bytes)
        .unwrap();
        let session = Session::start(
            engine.clone(),
            orb.into(),
            crate::io::SourceIo::new(
                &engine.providers,
                &[],
                &crate::Grants::default(),
                &crate::Cli::default(),
            )
            .unwrap(),
        )
        .unwrap();
        (engine, session)
    }

    /// Source factories cannot enter through dispatch, and refusals do not corrupt declarations.
    #[test]
    #[ignore = "requires TEMBRA_WASM_ENGINE"]
    fn source_commands_cannot_cross_execution_entrypoints() {
        let (_engine, session) = tick_bounded_session("entrypoints");
        let contract = Contract::from_source("checks.wit", "package example:entrypoints; interface checks { run: func() -> u32; } world test { export checks; }", None).unwrap();
        session.load(r#"globalThis.__tembraOrb={create(){return {value:7}},services:{checks:{methods:{run(environment){return environment.value}}}}};"#,contract.describe()).unwrap();
        let create =
            serde_json::json!({"kind":"create","environment":1,"service":null}).to_string();
        let invoke = serde_json::json!({"kind":"invoke","environment":1,"service":"checks","method":"run","args":[]}).to_string();
        let no_host =
            |_: &str, _: bool| -> tembra::Result<Value> { panic!("no host requests expected") };
        let error = session
            .blocking(Operation::Start, create.clone(), no_host)
            .unwrap_err();
        assert!(
            error.message.contains("Unknown interpreter command"),
            "{error}"
        );
        let error = session
            .blocking(Operation::Declare, invoke.clone(), no_host)
            .unwrap_err();
        assert!(
            error.message.contains("Only factory declarations"),
            "{error}"
        );
        session
            .blocking(Operation::Declare, create, no_host)
            .unwrap();
        assert_eq!(
            session.blocking(Operation::Start, invoke, no_host).unwrap(),
            serde_json::json!(7)
        );
    }

    /// Quarantine actual JavaScript tick exhaustion even when it returns without a Wasm trap.
    #[test]
    #[ignore = "requires TEMBRA_WASM_ENGINE"]
    fn exhausted_guest_with_queued_reactions_is_quarantined() {
        let (engine, session) = tick_bounded_session("exhausted");
        let contract = Contract::from_source("checks.wit", "package example:exhausted; interface checks { run: func() -> u32; } world test { export checks; }", None).unwrap();
        let source = r#"globalThis.__tembraOrb={services:{checks:{methods:{run(){
          for(let i=0;i<4000;i++) Promise.resolve().then(()=>i+1);
          while(true){}
        }}}}};"#;
        session.blocking(Operation::Load, serde_json::json!({"source":source,"heap_bytes":64<<20,"contract":contract.describe(),"instruction_ticks":50}).to_string(), |_,_| Ok(Value::Null)).unwrap();
        let error = session.blocking(Operation::Start, serde_json::json!({"kind":"invoke","environment":1,"service":"checks","method":"run","args":[]}).to_string(), |_,_| Ok(Value::Null)).unwrap_err();
        assert!(error.to_string().contains("interrupted"), "{error}");
        assert!(
            !error.to_string().contains("interpreter trap"),
            "test must reach the guest tick limit: {error}"
        );
        let error = session.begin(Operation::Start, "{}".into()).err().unwrap();
        assert_eq!(error.reason, "wasm_executor_quarantined");
        drop(session);
        assert_eq!(engine.instances(), 0);
    }

    /// Cleanup after a settled callback receives the configured tick bound, not the default.
    #[test]
    #[ignore = "requires TEMBRA_WASM_ENGINE"]
    fn cleanup_uses_the_configured_instruction_bound() {
        let (engine, session) = tick_bounded_session("cleanup");
        let contract = Contract::from_source("checks.wit", "package example:cleanup; interface checks { run: func() -> u32; } world test { export checks; }", None).unwrap();
        // The rejection handler runs only when cleanup revokes the unsent wait effect. Its loop
        // fits the default budget but not the configured one.
        let source = r#"globalThis.__tembraOrb={services:{checks:{methods:{run(){
          tembra.wait(1).catch(()=>{for(let i=0;i<5000000;i++){}});
          return 1;
        }}}}};"#;
        let load = crate::protocol::Load {
            source: source.into(),
            heap_bytes: 64 << 20,
            contract: contract.describe(),
            instruction_ticks: 50,
            builtins: Default::default(),
        };
        session
            .blocking(
                Operation::Load,
                serde_json::to_string(&load).unwrap(),
                |_, _| Ok(Value::Null),
            )
            .unwrap();
        let error = session.blocking(Operation::Start, serde_json::json!({"kind":"invoke","environment":1,"service":"checks","method":"run","args":[]}).to_string(), |_,_| Ok(Value::Null)).unwrap_err();
        // The default budget would let cleanup finish and return `Ok(1)`.
        assert!(error.to_string().contains("interrupted"), "{error}");
        assert!(
            !error.to_string().contains("interpreter trap"),
            "test must reach the guest tick limit: {error}"
        );
        let error = session.begin(Operation::Start, "{}".into()).err().unwrap();
        assert_eq!(error.reason, "wasm_executor_quarantined");
        drop(session);
        assert_eq!(engine.instances(), 0);
    }
}
