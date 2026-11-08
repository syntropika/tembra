//! Keep a compiled component Store isolated and physically owned until each native call settles.
use super::values;
use crate::{Engine, failure};
use futures::channel::oneshot;
use serde_json::Value;
use std::{
    sync::{Arc, Mutex, atomic::Ordering, mpsc},
    thread,
};
use tembra::Result;
use wasmtime::{
    Store, StoreLimits, StoreLimitsBuilder,
    component::{Component, Linker, ResourceTable, Val},
};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

struct Call {
    interface: String,
    method: String,
    arguments: Value,
    reply: oneshot::Sender<Result<Value>>,
}

pub(super) struct Session {
    commands: Option<mpsc::Sender<Call>>,
    thread: Option<thread::JoinHandle<()>>,
    poison: Arc<Mutex<Option<String>>>,
    admission: futures::lock::Mutex<()>,
    packet_bytes: usize,
}

impl Session {
    /// Instantiate the compiled code once with this Orb revision's own standard providers.
    pub(super) fn start(
        engine: Engine,
        component: Component,
        mut wasi: WasiCtxBuilder,
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
                let _count = Count(engine.active_instances.clone());
                let outcome = (|| -> wasmtime::Result<()> {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()?;
                    runtime.block_on(async {
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
                            linker.instantiate_async(&mut store, &component),
                        )
                        .await
                        .map_err(|_| {
                            wasmtime::Error::msg("component initialization timed out")
                        })??;
                        let _ = ready.send(Ok(()));
                        while let Ok(call) = receive.recv() {
                            store.set_fuel(engine.limits.fuel)?;
                            let result = async {
                                let (_, interface) = instance
                                    .get_export(&mut store, None, &call.interface)
                                    .ok_or_else(|| {
                                        wasmtime::Error::msg("missing component interface")
                                    })?;
                                let (_, index) = instance
                                    .get_export(&mut store, Some(&interface), &call.method)
                                    .ok_or_else(|| {
                                        wasmtime::Error::msg("missing component method")
                                    })?;
                                let function =
                                    instance.get_func(&mut store, index).ok_or_else(|| {
                                        wasmtime::Error::msg("component export is not a function")
                                    })?;
                                let ty = function.ty(&store);
                                let args = call.arguments.as_array().ok_or_else(|| {
                                    wasmtime::Error::msg("component arguments require a tuple")
                                })?;
                                if args.len() != ty.params().len() || ty.results().len() > 1 {
                                    return Err(wasmtime::Error::msg(
                                        "component function arity differs",
                                    ));
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
                                    .call_async(&mut store, &arguments, &mut results)
                                    .await?;
                                let value = match results
                                    .into_iter()
                                    .next()
                                    .map(values::lift)
                                    .transpose()
                                {
                                    Ok(value) => value.unwrap_or(Value::Null),
                                    Err(error) => return Ok(Err(error)),
                                };
                                if value.to_string().len() > engine.limits.packet_bytes {
                                    return Ok(Err(failure(
                                        "component result exceeds the packet limit",
                                    )));
                                }
                                Ok(Ok(value))
                            };
                            let result = tokio::time::timeout(
                                std::time::Duration::from_millis(engine.limits.timeout_ms),
                                result,
                            )
                            .await
                            .map_err(|_| wasmtime::Error::msg("component call timed out"))
                            .and_then(|result| result);
                            match result {
                                Ok(result) => {
                                    let _ = call.reply.send(result);
                                }
                                Err(error) => {
                                    let message = format!("component trap: {error:#}");
                                    *worker_poison.lock().expect("poison state") =
                                        Some(message.clone());
                                    drop(store);
                                    let _ = call.reply.send(Err(failure(message)));
                                    return Ok(());
                                }
                            }
                        }
                        Ok(())
                    })
                })();
                if let Err(error) = outcome {
                    let _ = ready.send(Err(format!("component initialization failed: {error:#}")));
                }
            })
            .map_err(failure)?;
        match initialized.recv().map_err(failure)? {
            Ok(()) => Ok(Self {
                commands: Some(send),
                thread: Some(thread),
                poison,
                admission: futures::lock::Mutex::new(()),
                packet_bytes,
            }),
            Err(error) => {
                drop(send);
                let _ = thread.join();
                Err(failure(error))
            }
        }
    }

    /// Serialize calls while the native host retains their physical invocation pins.
    pub(super) async fn call(
        &self,
        interface: String,
        method: String,
        arguments: Value,
    ) -> Result<Value> {
        let _permit = self.admission.lock().await;
        if let Some(error) = self.poison.lock().expect("poison state").as_ref() {
            return Err(failure(error));
        }
        if arguments.to_string().len() > self.packet_bytes {
            return Err(failure("component arguments exceed the packet limit"));
        }
        let (reply, receive) = oneshot::channel();
        self.commands
            .as_ref()
            .expect("live session")
            .send(Call {
                interface,
                method,
                arguments,
                reply,
            })
            .map_err(failure)?;
        receive.await.map_err(failure)?
    }
}

impl Drop for Session {
    /// Join only this Store after its last kernel-owned callback owner has been released.
    fn drop(&mut self) {
        self.commands.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
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
struct Count(Arc<std::sync::atomic::AtomicUsize>);
impl Drop for Count {
    /// Retire the metric after the Store is dropped, including instantiation failure.
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Release);
    }
}
