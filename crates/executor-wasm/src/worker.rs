//! Keep Wasmtime on an owned worker; only scope-bound requests cross to the native host.
use crate::{Engine, failure};
use async_channel::{Receiver, Sender};
use futures::{FutureExt, StreamExt, stream::FuturesUnordered};
use serde_json::Value;
use std::sync::{Arc, atomic::Ordering};
use std::{
    cell::Cell,
    rc::Rc,
    sync::{Mutex, mpsc},
    thread,
};
use tembra::Result;
use tokio::sync::oneshot;
use wasmtime_wasi::WasiCtxBuilder;

mod diagnostics;
mod runtime;
mod steps;

pub(crate) enum Event {
    Host {
        packet: String,
        asynchronous: bool,
        reply: oneshot::Sender<std::result::Result<String, String>>,
    },
    Done(std::result::Result<String, String>),
}

struct Command {
    export: &'static str,
    packet: String,
    events: Sender<Event>,
}

struct Owner {
    commands: Option<mpsc::Sender<Command>>,
    thread: Option<thread::JoinHandle<()>>,
    busy: Cell<bool>,
    packet_bytes: usize,
    admission: Arc<futures::lock::Mutex<()>>,
    poison: Arc<Mutex<Option<String>>>,
    identity: u64,
    heap_bytes: usize,
    diagnostics: diagnostics::Diagnostics,
    admission_ms: u64,
}

impl Drop for Owner {
    /// Close the command stream and join only this interpreter's worker.
    fn drop(&mut self) {
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
    /// Start an owned Store with no filesystem preopens, environment or socket grants.
    pub(crate) fn start(engine: Engine, wasi: WasiCtxBuilder, orb: String) -> Result<Self> {
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
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let identity = NEXT
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .map_err(|_| failure("interpreter identity space exhausted"))?;
        let thread = thread::Builder::new()
            .name("tembra-wasm".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                if let Ok(runtime) = runtime {
                    let terminal = runtime.block_on(runtime::run(
                        engine,
                        receive,
                        wasi,
                        worker_poison,
                        worker_diagnostics,
                    ));
                    publish_after_shutdown(runtime, terminal);
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
            identity,
            heap_bytes,
            diagnostics,
            admission_ms,
        })))
    }

    /// Identify this isolated revision for causal reentry checks in trusted context metadata.
    pub(crate) fn identity(&self) -> u64 {
        self.0.identity
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
    pub(crate) fn load(&self, source: &str, contract: impl serde::Serialize) -> Result<Value> {
        #[derive(serde::Serialize)]
        struct Load<'a, T> {
            source: &'a str,
            heap_bytes: usize,
            contract: T,
        }
        self.blocking(
            "load",
            serde_json::to_string(&Load {
                source,
                heap_bytes: self.0.heap_bytes,
                contract,
            })
            .map_err(failure)?,
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
            export: "dispatch",
            packet: serde_json::to_string(&crate::protocol::GuestCommand::Release { environment })
                .expect("serializable release"),
            events,
        };
        if let Some(sender) = &self.0.commands {
            let _ = sender.send(command);
        }
    }

    /// Submit one callback, refusing reentry before it could deadlock on this Store.
    pub(crate) fn begin(&self, export: &'static str, packet: String) -> Result<Receiver<Event>> {
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
            export,
            packet,
            events: send,
        };
        if self
            .0
            .commands
            .as_ref()
            .expect("live owner")
            .send(command)
            .is_err()
        {
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
        events.close();
        while let Ok(event) = events.try_recv() {
            drop(event);
        }
        self.0
            .poison
            .lock()
            .expect("poison state")
            .get_or_insert_with(|| {
                "platform callback driver ended before physical settlement".into()
            });
    }

    /// Execute a synchronous declaration while servicing only immediate scope operations.
    pub(crate) fn blocking(
        &self,
        export: &'static str,
        packet: String,
        mut host: impl FnMut(&str, bool) -> Result<Value>,
    ) -> Result<Value> {
        let events = self.begin(export, packet)?;
        let mut guard = Busy {
            owner: &self.0,
            events: &events,
            completed: false,
        };
        loop {
            match events.recv_blocking().map_err(failure)? {
                Event::Done(result) => {
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
                    let _ = reply.send(encode(result, self.0.packet_bytes));
                }
            }
        }
    }

    /// Keep the callback physically owned until guest completion and requested host work settle.
    pub(crate) async fn dispatch(
        &self,
        packet: impl serde::Serialize,
        host: impl AsyncFn(&str, bool) -> Result<Value>,
    ) -> Result<Value> {
        let _admission = self.0.admission.lock().await;
        let events = self.begin("dispatch", serde_json::to_string(&packet).map_err(failure)?)?;
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
                                let operation = std::pin::pin!(host(&packet, asynchronous));
                                let closed = std::pin::pin!(reply.closed());
                                match futures::future::select(operation, closed).await {
                                    futures::future::Either::Left((result, _)) => Some(result),
                                    futures::future::Either::Right(_) => None,
                                }
                            };
                            if let Some(result) = result {
                                let _ = reply.send(encode(result, self.0.packet_bytes));
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
            self.events.close();
            while let Ok(event) = self.events.try_recv() {
                drop(event);
            }
            self.owner
                .poison
                .lock()
                .expect("poison state")
                .get_or_insert_with(|| {
                    "interpreter callback driver ended before physical settlement".into()
                });
        }
    }
}

/// Runtime destruction joins WASI blocking work before a quarantine becomes observable.
fn publish_after_shutdown(
    runtime: tokio::runtime::Runtime,
    terminal: Vec<(Sender<Event>, String)>,
) {
    drop(runtime);
    for (events, error) in terminal {
        let _ = events.send_blocking(Event::Done(Err(error)));
    }
}

/// Parse guest output only after preserving boundary diagnostics.
fn decode(result: std::result::Result<String, String>) -> Result<Value> {
    serde_json::from_str(&result.map_err(failure)?).map_err(failure)
}

/// Encode portable data without transferring native handles or callbacks.
fn encode(result: Result<Value>, limit: usize) -> std::result::Result<String, String> {
    let value = result.map_err(|error| error.to_string())?.to_string();
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
            publish_after_shutdown(runtime, vec![(events, "quarantined".into())]);
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
        let session =
            Session::start(engine.clone(), WasiCtxBuilder::new(), "abandoned".into()).unwrap();
        let contract = Contract::from_source("checks.wit","package example:abandon; interface checks { run: func() -> u32; } world test { export checks; }",None).unwrap();
        session.load(r#"globalThis.__tembraOrb={services:{checks:{create(){__hostSync('{"operation":"state.read"}');return {}},methods:{run(){return 1}}}}};"#,contract.describe()).unwrap();
        let events = session
            .begin(
                "dispatch",
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

    /// Quarantine actual JavaScript tick exhaustion even when it returns without a Wasm trap.
    #[test]
    #[ignore = "requires TEMBRA_WASM_ENGINE"]
    fn exhausted_guest_with_queued_reactions_is_quarantined() {
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
        let session =
            Session::start(engine.clone(), WasiCtxBuilder::new(), "exhausted".into()).unwrap();
        let contract = Contract::from_source("checks.wit", "package example:exhausted; interface checks { run: func() -> u32; } world test { export checks; }", None).unwrap();
        let source = r#"globalThis.__tembraOrb={services:{checks:{methods:{run(){
          for(let i=0;i<4000;i++) Promise.resolve().then(()=>i+1);
          while(true){}
        }}}}};"#;
        session.blocking("load", serde_json::json!({"source":source,"heap_bytes":64<<20,"contract":contract.describe(),"instruction_ticks":50}).to_string(), |_,_| Ok(Value::Null)).unwrap();
        let error = session.blocking("dispatch", serde_json::json!({"kind":"invoke","environment":1,"service":"checks","method":"run","args":[]}).to_string(), |_,_| Ok(Value::Null)).unwrap_err();
        assert!(error.to_string().contains("interrupted"), "{error}");
        assert!(
            !error.to_string().contains("interpreter trap"),
            "test must reach the guest tick limit: {error}"
        );
        let error = session.begin("dispatch", "{}".into()).err().unwrap();
        assert_eq!(error.reason, "wasm_executor_quarantined");
        drop(session);
        assert_eq!(engine.instances(), 0);
    }
}
