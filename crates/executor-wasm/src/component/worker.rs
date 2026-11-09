//! Keep a compiled component Store isolated and physically owned until each native call settles.
use super::values;
use crate::{Engine, failure};
use serde_json::Value;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
};
use tembra::Result;
use wasmtime::{
    Store, StoreLimits, StoreLimitsBuilder,
    component::{Component, Instance, Linker, ResourceTable, Val},
};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

/// Receive one call result on the component worker, or on the submitter when no Store accepts it.
pub(super) type Complete = Box<dyn FnOnce(Result<Value>) + Send>;

struct Call {
    interface: String,
    method: String,
    arguments: Value,
    reply: Reply,
}

/// Settle each submitted call exactly once, including calls that no Store will execute.
struct Reply {
    complete: Option<Complete>,
    poison: Arc<Mutex<Option<String>>>,
}

impl Reply {
    /// Publish one result to the submitter's completion.
    fn send(mut self, result: Result<Value>) {
        if let Some(complete) = self.complete.take() {
            complete(result);
        }
    }
}

impl Drop for Reply {
    /// Refuse a discarded call with the worker's terminal diagnostic.
    fn drop(&mut self) {
        if let Some(complete) = self.complete.take() {
            let poison = self.poison.lock().ok().and_then(|poison| poison.clone());
            complete(Err(failure(
                poison.unwrap_or_else(|| "component worker stopped".into()),
            )));
        }
    }
}

/// A failed call whose reply must wait until the Store and its runtime have stopped.
struct Terminal {
    reply: Reply,
    message: String,
}

pub(super) struct Session {
    commands: Option<mpsc::Sender<Call>>,
    thread: Option<thread::JoinHandle<()>>,
    poison: Arc<Mutex<Option<String>>>,
    packet_bytes: usize,
}

impl Session {
    /// Instantiate the compiled code once with this Orb revision's own standard providers.
    pub(super) fn start(
        engine: Engine,
        component: Component,
        wasi: WasiCtxBuilder,
    ) -> Result<Self> {
        let (send, receive) = mpsc::channel::<Call>();
        let (ready, initialized) = mpsc::channel::<std::result::Result<(), String>>();
        let poison = Arc::new(Mutex::new(None));
        let worker_poison = poison.clone();
        let packet_bytes = engine.limits.packet_bytes;
        let thread = thread::Builder::new()
            .name("tembra-component".into())
            .spawn(move || {
                engine.active_instances.fetch_add(1, Ordering::Release);
                let count = Count(engine.active_instances.clone());
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        drop(count);
                        let _ =
                            ready.send(Err(format!("component initialization failed: {error:#}")));
                        return;
                    }
                };
                let outcome = runtime.block_on(serve(&engine, &component, wasi, &ready, &receive));
                match outcome {
                    Ok(terminal) => {
                        publish_after_shutdown(runtime, count, terminal, &worker_poison, receive);
                    }
                    Err(error) => {
                        drop(runtime);
                        drop(count);
                        let _ =
                            ready.send(Err(format!("component initialization failed: {error:#}")));
                    }
                }
            })
            .map_err(failure)?;
        match initialized.recv().map_err(failure)? {
            Ok(()) => Ok(Self {
                commands: Some(send),
                thread: Some(thread),
                poison,
                packet_bytes,
            }),
            Err(error) => {
                drop(send);
                let _ = thread.join();
                Err(failure(error))
            }
        }
    }

    /// Queue one call in order; the dedicated worker publishes its result without a waiting thread.
    pub(super) fn submit(
        &self,
        interface: String,
        method: String,
        arguments: Value,
        complete: Complete,
    ) {
        let reply = Reply {
            complete: Some(complete),
            poison: self.poison.clone(),
        };
        let poison = self.poison.lock().expect("poison state").clone();
        if let Some(error) = poison {
            reply.send(Err(failure(error)));
            return;
        }
        if arguments.to_string().len() > self.packet_bytes {
            reply.send(Err(failure("component arguments exceed the packet limit")));
            return;
        }
        let call = Call {
            interface,
            method,
            arguments,
            reply,
        };
        // A stopped worker returns the call; dropping it refuses the call with the terminal diagnostic.
        if let Some(commands) = &self.commands {
            let _ = commands.send(call);
        }
    }
}

impl Drop for Session {
    /// Join only this Store after its last owner is released.
    fn drop(&mut self) {
        self.commands.take();
        if let Some(thread) = self.thread.take() {
            // A completion can release the last owner on the worker itself.
            // The closed channel then ends that worker after the completion returns.
            if thread.thread().id() != thread::current().id() {
                let _ = thread.join();
            }
        }
    }
}

/// Instantiate once, then execute queued calls until the owner closes or one call fails terminally.
async fn serve(
    engine: &Engine,
    component: &Component,
    mut wasi: WasiCtxBuilder,
    ready: &mpsc::Sender<std::result::Result<(), String>>,
    receive: &mpsc::Receiver<Call>,
) -> wasmtime::Result<Option<Terminal>> {
    let mut linker = Linker::<State>::new(&engine.engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    let mut store = Store::new(
        &engine.engine,
        State {
            wasi: wasi.build(),
            table: ResourceTable::new(),
            limits: StoreLimitsBuilder::new()
                .memory_size(engine.limits.memory_bytes)
                .build(),
        },
    );
    store.limiter(|state| &mut state.limits);
    store.set_fuel(engine.limits.fuel)?;
    store.fuel_async_yield_interval(Some(10_000))?;
    let instance = tokio::time::timeout(
        std::time::Duration::from_millis(engine.limits.timeout_ms),
        linker.instantiate_async(&mut store, component),
    )
    .await
    .map_err(|_| wasmtime::Error::msg("component initialization timed out"))??;
    let _ = ready.send(Ok(()));
    while let Ok(call) = receive.recv() {
        let Call {
            interface,
            method,
            arguments,
            reply,
        } = call;
        let result = match store.set_fuel(engine.limits.fuel) {
            Ok(()) => tokio::time::timeout(
                std::time::Duration::from_millis(engine.limits.timeout_ms),
                execute(
                    engine, &mut store, &instance, &interface, &method, &arguments,
                ),
            )
            .await
            .map_err(|_| wasmtime::Error::msg("component call timed out"))
            .and_then(|result| result),
            Err(error) => Err(error),
        };
        match result {
            Ok(result) => reply.send(result),
            Err(error) => {
                // The Store is dropped when this future returns; the reply waits for runtime shutdown.
                return Ok(Some(Terminal {
                    reply,
                    message: format!("component trap: {error:#}"),
                }));
            }
        }
    }
    Ok(None)
}

/// Run one export; the outer error is terminal, the inner error refuses only this call.
async fn execute(
    engine: &Engine,
    store: &mut Store<State>,
    instance: &Instance,
    interface: &str,
    method: &str,
    arguments: &Value,
) -> wasmtime::Result<Result<Value>> {
    let (_, exported) = instance
        .get_export(&mut *store, None, interface)
        .ok_or_else(|| wasmtime::Error::msg("missing component interface"))?;
    let (_, index) = instance
        .get_export(&mut *store, Some(&exported), method)
        .ok_or_else(|| wasmtime::Error::msg("missing component method"))?;
    let function = instance
        .get_func(&mut *store, index)
        .ok_or_else(|| wasmtime::Error::msg("component export is not a function"))?;
    let ty = function.ty(&*store);
    let args = arguments
        .as_array()
        .ok_or_else(|| wasmtime::Error::msg("component arguments require a tuple"))?;
    if args.len() != ty.params().len() || ty.results().len() > 1 {
        return Err(wasmtime::Error::msg("component function arity differs"));
    }
    let arguments = match ty
        .params()
        .zip(args)
        .map(|((_, ty), value)| values::lower(ty, value))
        .collect::<Result<Vec<_>>>()
    {
        Ok(arguments) => arguments,
        Err(error) => return Ok(Err(error)),
    };
    let mut results = vec![Val::Bool(false); ty.results().len()];
    function
        .call_async(&mut *store, &arguments, &mut results)
        .await?;
    let value = match results.into_iter().next().map(values::lift).transpose() {
        Ok(value) => value.unwrap_or(Value::Null),
        Err(error) => return Ok(Err(error)),
    };
    if value.to_string().len() > engine.limits.packet_bytes {
        return Ok(Err(failure("component result exceeds the packet limit")));
    }
    Ok(Ok(value))
}

/// Runtime destruction joins WASI blocking work before any call can observe the quarantine.
fn publish_after_shutdown(
    runtime: tokio::runtime::Runtime,
    count: Count,
    terminal: Option<Terminal>,
    poison: &Mutex<Option<String>>,
    queued: mpsc::Receiver<Call>,
) {
    drop(runtime);
    drop(count);
    if let Some(Terminal { reply, message }) = terminal {
        *poison.lock().expect("poison state") = Some(message.clone());
        reply.send(Err(failure(message)));
    }
    // Refuse queued calls in order. Calls sent after this drain are discarded by the closed channel.
    while let Ok(call) = queued.try_recv() {
        drop(call);
    }
}

struct State {
    wasi: WasiCtx,
    table: ResourceTable,
    limits: StoreLimits,
}

impl WasiView for State {
    /// Expose this component's independently granted WASI context.
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

struct Count(Arc<AtomicUsize>);

impl Drop for Count {
    /// Retire the metric after the Store and its runtime are dropped, including instantiation failure.
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Limits;
    use std::time::Duration;

    /// The smallest valid component: it instantiates and exports nothing.
    const EMPTY: &[u8] = b"\0asm\x0d\0\x01\0";

    /// Capture one completion on a channel.
    fn capture() -> (Complete, mpsc::Receiver<Result<Value>>) {
        let (send, receive) = mpsc::channel();
        (
            Box::new(move |result| {
                let _ = send.send(result);
            }),
            receive,
        )
    }

    /// Start a real Store whose every call fails terminally.
    fn empty_session(engine: &Engine) -> Session {
        let component = Component::new(&engine.engine, EMPTY).unwrap();
        Session::start(engine.clone(), component, WasiCtxBuilder::new()).unwrap()
    }

    /// A detached WASI blocking operation must finish before the trap reply or poison is published.
    #[test]
    fn trap_reply_waits_for_blocking_runtime_work() {
        let (started, entered) = mpsc::channel();
        let (release, blocked) = mpsc::channel::<()>();
        let (complete, observed) = capture();
        let (queued_complete, queued_observed) = capture();
        let poison = Arc::new(Mutex::new(None));
        let worker_poison = poison.clone();
        let instances = Arc::new(AtomicUsize::new(1));
        let count = Count(instances.clone());
        let (queue, queued) = mpsc::channel();
        queue
            .send(Call {
                interface: String::new(),
                method: String::new(),
                arguments: Value::Null,
                reply: Reply {
                    complete: Some(queued_complete),
                    poison: poison.clone(),
                },
            })
            .unwrap();
        let terminal = Terminal {
            reply: Reply {
                complete: Some(complete),
                poison: poison.clone(),
            },
            message: "component trap: timed out".into(),
        };
        let worker = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.spawn_blocking(move || {
                let _ = started.send(());
                let _ = blocked.recv();
            });
            publish_after_shutdown(runtime, count, Some(terminal), &worker_poison, queued);
        });
        entered.recv().unwrap();
        let premature = observed.recv_timeout(Duration::from_millis(100));
        let early_poison = poison.lock().unwrap().clone();
        let early_instances = instances.load(Ordering::Acquire);
        release.send(()).unwrap();
        worker.join().unwrap();
        assert!(
            premature.is_err(),
            "trap reply preceded physical completion"
        );
        assert_eq!(
            early_poison, None,
            "quarantine preceded physical completion"
        );
        assert_eq!(early_instances, 1, "Store metric retired before shutdown");
        let error = observed.recv().unwrap().unwrap_err();
        assert!(error.message.contains("timed out"), "{error}");
        let queued = queued_observed.recv().unwrap().unwrap_err();
        assert!(queued.message.contains("timed out"), "{queued}");
        assert_eq!(instances.load(Ordering::Acquire), 0);
    }

    /// Queued calls receive the quarantine diagnostic; none waits on a thread or stays unsettled.
    #[test]
    fn queued_calls_settle_after_quarantine() {
        let engine = Engine::new(Limits::default()).unwrap();
        let session = empty_session(&engine);
        assert_eq!(engine.instances(), 1);
        let receivers = (0..3)
            .map(|_| {
                let (complete, receive) = capture();
                session.submit(
                    "missing".into(),
                    "call".into(),
                    Value::Array(vec![]),
                    complete,
                );
                receive
            })
            .collect::<Vec<_>>();
        for receive in receivers {
            let error = receive
                .recv_timeout(Duration::from_secs(10))
                .expect("queued call settled")
                .unwrap_err();
            assert!(
                error.message.contains("missing component interface"),
                "{error}"
            );
        }
        let (complete, late) = capture();
        session.submit(
            "missing".into(),
            "call".into(),
            Value::Array(vec![]),
            complete,
        );
        let error = late
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap_err();
        assert!(
            error.message.contains("missing component interface"),
            "{error}"
        );
        drop(session);
        assert_eq!(engine.instances(), 0);
    }

    /// Oversized arguments are refused before they reach the Store.
    #[test]
    fn oversized_arguments_do_not_poison_the_store() {
        let engine = Engine::new(Limits {
            packet_bytes: 8,
            ..Limits::default()
        })
        .unwrap();
        let session = empty_session(&engine);
        let (complete, receive) = capture();
        session.submit(
            "missing".into(),
            "call".into(),
            Value::String("exceeds the packet".into()),
            complete,
        );
        let error = receive.recv().unwrap().unwrap_err();
        assert!(error.message.contains("packet limit"), "{error}");
        assert_eq!(*session.poison.lock().unwrap(), None);
    }

    /// A completion may release the last owner on the worker without joining that worker.
    #[test]
    fn completion_releases_last_owner_on_the_worker() {
        let engine = Engine::new(Limits::default()).unwrap();
        let session = Arc::new(empty_session(&engine));
        let held = session.clone();
        let (entered, inside) = mpsc::channel();
        let (release, gate) = mpsc::channel::<()>();
        let (finished, done) = mpsc::channel();
        session.submit(
            "missing".into(),
            "call".into(),
            Value::Array(vec![]),
            Box::new(move |result| {
                let _ = entered.send(result.is_err());
                let _ = gate.recv();
                drop(held);
                let _ = finished.send(thread::current().name().map(str::to_owned));
            }),
        );
        assert!(inside.recv_timeout(Duration::from_secs(10)).unwrap());
        drop(session);
        release.send(()).unwrap();
        let name = done
            .recv_timeout(Duration::from_secs(10))
            .expect("the worker released its last owner without joining itself");
        assert_eq!(name.as_deref(), Some("tembra-component"));
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while engine.instances() != 0 && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(engine.instances(), 0);
    }
}
