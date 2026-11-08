//! Own the Wasmtime Store and its standard WASI imports on one worker thread.
use super::{Command, Event};
use crate::Engine;
use async_channel::Sender;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
    mpsc,
};
use tokio::sync::oneshot;
use wasmtime::{
    Store, StoreContextMut, StoreLimits, StoreLimitsBuilder,
    component::{Linker, ResourceTable},
};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

pub(super) struct State {
    diagnostics: super::diagnostics::Diagnostics,
    wasi: WasiCtx,
    pub(super) table: ResourceTable,
    limits: StoreLimits,
    pub(super) events: Option<Sender<Event>>,
    packet_bytes: usize,
    deadline: Option<std::time::Instant>,
}

struct InstanceCount(Arc<AtomicUsize>);
impl Drop for InstanceCount {
    /// Retire the instance metric after its Store and pending imports have been dropped.
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Release);
    }
}
impl WasiView for State {
    /// Supply the per-instance WASI context with no ambient capabilities.
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

/// Dispatch a request through the current invocation's channel, never a guest-selected scope.
pub(super) fn request(
    state: &State,
    packet: String,
    asynchronous: bool,
) -> std::result::Result<oneshot::Receiver<std::result::Result<String, String>>, String> {
    if packet.len() > state.packet_bytes {
        return Err("host packet limit exceeded".into());
    }
    let (reply, receive) = oneshot::channel();
    state
        .events
        .as_ref()
        .ok_or("no active invocation")?
        .try_send(Event::Host {
            packet,
            asynchronous,
            reply,
        })
        .map_err(|_| "host operation queue is full or closed")?;
    Ok(receive)
}

/// Instantiate once, run commands serially, and quarantine the Store after a trap.
pub(super) async fn run(
    engine: Engine,
    commands: mpsc::Receiver<Command>,
    mut wasi: WasiCtxBuilder,
    poison: Arc<Mutex<Option<String>>>,
    diagnostics: super::diagnostics::Diagnostics,
) -> Vec<(Sender<Event>, String)> {
    engine.active_instances.fetch_add(1, Ordering::Release);
    let _instance = InstanceCount(engine.active_instances.clone());
    let outcome = async {
        let mut linker = Linker::<State>::new(&engine.engine);
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
        linker.root().func_wrap(
            "host-sync",
            |store: StoreContextMut<State>, (packet,): (String,)| {
                if packet.len() > store.data().packet_bytes {
                    return Ok((Err("host packet limit exceeded".into()),));
                }
                if let Some(result) = store.data().diagnostics.accept(&packet) {
                    return Ok((result
                        .map(|value| value.to_string())
                        .map_err(|error| error.to_string()),));
                }
                let reply = match request(store.data(), packet, false) {
                    Ok(reply) => {
                        let remaining = store
                            .data()
                            .deadline
                            .map(|deadline| {
                                deadline.saturating_duration_since(std::time::Instant::now())
                            })
                            .unwrap_or_default();
                        let reply = std::pin::pin!(reply);
                        let deadline = std::pin::pin!(futures_timer::Delay::new(remaining));
                        match futures::executor::block_on(futures::future::select(reply, deadline))
                        {
                            futures::future::Either::Left((reply, _)) => {
                                reply.unwrap_or_else(|_| Err("host callback scope ended".into()))
                            }
                            futures::future::Either::Right(_) => {
                                Err("host callback reply exceeded its deadline".into())
                            }
                        }
                    }
                    Err(error) => Err(error),
                };
                Ok((reply,))
            },
        )?;
        let mut store = Store::new(
            &engine.engine,
            State {
                diagnostics,
                wasi: wasi.build(),
                table: ResourceTable::new(),
                limits: StoreLimitsBuilder::new()
                    .memory_size(engine.limits.memory_bytes)
                    .build(),
                events: None,
                packet_bytes: engine.limits.packet_bytes,
                deadline: None,
            },
        );
        store.limiter(|state| &mut state.limits);
        store.set_fuel(engine.limits.fuel)?;
        store.fuel_async_yield_interval(Some(10_000))?;
        let instance = linker
            .instantiate_async(
                &mut store,
                engine.component.as_ref().ok_or_else(|| {
                    wasmtime::Error::msg("source Orbs require a configured interpreter")
                })?,
            )
            .await?;
        let driver = super::steps::Driver::new(&instance, &mut store)?;
        while let Ok(command) = commands.recv() {
            store.data_mut().events = Some(command.events.clone());
            store.data_mut().deadline = Some(
                std::time::Instant::now()
                    + std::time::Duration::from_millis(engine.limits.timeout_ms),
            );
            store.set_fuel(engine.limits.fuel)?;
            let result = tokio::time::timeout(
                std::time::Duration::from_millis(engine.limits.timeout_ms),
                driver.call(command.export, command.packet, &mut store),
            )
            .await
            .map_err(|_| {
                wasmtime::Error::msg(
                    "interpreter callback exceeded its wall time limit; Store quarantined",
                )
            })
            .and_then(|result| result);
            store.data_mut().events = None;
            let trapped = result.as_ref().map_or(true, |outcome| outcome.interrupted);
            let result = result
                .map(|outcome| outcome.value)
                .unwrap_or_else(|error| Err(format!("interpreter trap: {error:#}")))
                .and_then(|value| {
                    if value.len() <= engine.limits.packet_bytes {
                        Ok(value)
                    } else {
                        Err("guest result exceeds the configured packet limit".into())
                    }
                });
            if trapped {
                let error = result
                    .err()
                    .unwrap_or_else(|| "interpreter interrupted; Store quarantined".into());
                *poison.lock().expect("poison state") = Some(error.clone());
                drop(store);
                return Ok(vec![(command.events, error)]);
            }
            let _ = command.events.send(Event::Done(result)).await;
        }
        Ok::<_, wasmtime::Error>(Vec::new())
    }
    .await;
    match outcome {
        Ok(terminal) => terminal,
        Err(error) => commands
            .try_iter()
            .map(|command| {
                (
                    command.events,
                    format!("interpreter initialization failed: {error:#}"),
                )
            })
            .collect(),
    }
}
