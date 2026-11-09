//! Keep Wasmtime on an owned worker; only scope-bound requests cross to the native host.
//!
//! One session owns one Store per Orb revision. A source revision runs the interpreter core
//! module behind the private fixed-buffer ABI in `abi.rs`; its Store links no filesystem, socket
//! or environment capability. A compiled revision runs its component on the same owner and
//! speaks the same private host protocol (see `component::worker`).
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
mod runtime;
mod steps;
mod wasi;

pub(crate) use runtime::check;

pub(crate) enum Event {
    Host {
        packet: String,
        asynchronous: bool,
        reply: oneshot::Sender<std::result::Result<String, String>>,
    },
    Done(std::result::Result<String, String>),
}

/// One host-selected operation and the channel that carries its scoped requests and completion.
pub(crate) struct Command {
    pub(crate) operation: Operation,
    pub(crate) packet: String,
    pub(crate) events: Sender<Event>,
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
    /// Streams of the abandoned callback close first, so its pending socket work settles.
    fn abandon(&self, events: &Receiver<Event>, reason: &str) {
        self.io.streams().end_callback();
        close_replies(events);
        self.poison
            .lock()
            .expect("poison state")
            .get_or_insert_with(|| reason.into());
    }
}

impl Drop for Owner {
    /// Close the command stream and join only this interpreter's worker.
    /// No transport stream survives its Orb's host.
    fn drop(&mut self) {
        self.io.streams().end_callback();
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
    /// Share the transport table so a foreign event receiver can end the callback's epoch.
    pub(crate) fn streams(&self) -> crate::io::Streams {
        self.0.io.streams().clone()
    }

    pub(crate) fn io_operation(&self, packet: &str) -> Result<crate::IoOperation> {
        if packet.len() > self.0.packet_bytes {
            return Err(failure("host packet limit exceeded"));
        }
        self.0.io.operation(packet)
    }

    /// Start an owned Store with no filesystem preopens, environment or socket grants.
    pub(crate) fn start(engine: Engine, orb: String, io: crate::io::SourceIo) -> Result<Self> {
        let worker = engine.clone();
        Self::spawn(&engine, orb, io, move |commands, poison, diagnostics| {
            runtime::run(worker, commands, poison, diagnostics)
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
        let admission_ms = engine.limits.timeout_ms / 2;
        let poison = Arc::new(Mutex::new(None));
        let worker_poison = poison.clone();
        let instances = engine.active_instances.clone();
        let thread = thread::Builder::new()
            .name("tembra-wasm".into())
            .spawn(move || {
                instances.fetch_add(1, std::sync::atomic::Ordering::Release);
                let count = Count(instances);
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                if let Ok(runtime) = runtime {
                    let terminal =
                        runtime.block_on(run(receive, worker_poison.clone(), worker_diagnostics));
                    publish_after_shutdown(runtime, count, &worker_poison, terminal);
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
        let (events, receive) = async_channel::bounded(1);
        drop(receive);
        let command = Command {
            operation: Operation::Start,
            packet: serde_json::to_string(&crate::protocol::GuestCommand::Release { environment })
                .expect("serializable release"),
            events,
        };
        if let Some(sender) = &self.0.commands {
            let _ = sender.send(command);
        }
    }

    /// Submit one callback, refusing reentry before it could deadlock on this Store.
    pub(crate) fn begin(&self, operation: Operation, packet: String) -> Result<Receiver<Event>> {
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
        let (send, receive) = async_channel::bounded(128);
        let command = Command {
            operation,
            packet,
            events: send,
        };
        // Transport streams opened by this callback belong to its epoch until it completes.
        self.0.io.streams().begin_callback();
        if self
            .0
            .commands
            .as_ref()
            .expect("live owner")
            .send(command)
            .is_err()
        {
            self.0.io.streams().end_callback();
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
                Event::Done(result) => {
                    self.0.io.streams().end_callback();
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
    pub(crate) async fn dispatch(
        &self,
        _admission: futures::lock::OwnedMutexGuard<()>,
        packet: impl serde::Serialize,
        host: impl AsyncFn(&str, bool) -> Result<Value>,
    ) -> Result<Value> {
        let events = self.begin(
            Operation::Start,
            serde_json::to_string(&packet).map_err(failure)?,
        )?;
        let mut guard = Busy {
            owner: &self.0,
            events: &events,
            completed: false,
        };
        let mut pending = FuturesUnordered::new();
        loop {
            let event = if pending.is_empty() {
                events.recv().await
            } else {
                let event = std::pin::pin!(events.recv());
                let completed = std::pin::pin!(pending.next());
                match futures::future::select(completed, event).await {
                    futures::future::Either::Left(_) => continue,
                    futures::future::Either::Right((event, _)) => event,
                }
            };
            match event.map_err(failure)? {
                Event::Done(result) => {
                    // Close this callback's streams before waiting, so abandoned reads wake now
                    // instead of at their deadline; physical settlement is still awaited.
                    self.0.io.streams().end_callback();
                    while pending.next().await.is_some() {}
                    guard.completed = true;
                    return decode(result);
                }
                Event::Host {
                    packet,
                    asynchronous,
                    mut reply,
                } => {
                    let host = &host;
                    pending.push(
                        async move {
                            let result = {
                                let physical_io = asynchronous
                                    && crate::protocol::AsyncRequest::decode(&packet)
                                        .is_ok_and(|request| request.is_io());
                                let operation = std::pin::pin!(host(&packet, asynchronous));
                                if physical_io {
                                    Some(operation.await)
                                } else {
                                    let closed = std::pin::pin!(reply.closed());
                                    match futures::future::select(operation, closed).await {
                                        futures::future::Either::Left((result, _)) => Some(result),
                                        futures::future::Either::Right(_) => None,
                                    }
                                }
                            };
                            if let Some(result) = result {
                                let _ = reply.send(encode(
                                    result.map(|value| value.to_string()),
                                    self.0.packet_bytes,
                                ));
                            }
                        }
                        .boxed_local(),
                    );
                }
            }
        }
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
        let _ = events.send_blocking(Event::Done(Err(error)));
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
                Duration::from_secs(1),
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
            let (mut store, driver) = runtime::instantiate(&engine, diagnostics).await.unwrap();
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
            let (mut store, driver) = runtime::instantiate(&engine, diagnostics).await.unwrap();
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
                Duration::from_secs(1),
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
