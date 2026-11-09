//! Own the Wasmtime Store, its bounded imported memory and the interpreter's imports on one worker.
use super::{Command, Event};
use crate::{Engine, abi};
use async_channel::Sender;
use std::sync::{Arc, Mutex, mpsc};
use tokio::sync::oneshot;
use wasmtime::{
    Caller, ExternType, Linker, Memory, MemoryType, Module, Store, StoreLimits, StoreLimitsBuilder,
};

pub(super) struct State {
    pub(super) diagnostics: super::diagnostics::Diagnostics,
    pub(super) wasi: super::wasi::Wasi,
    /// The instance's only linear memory, created by the host with the configured maximum.
    pub(super) memory: Option<Memory>,
    limits: StoreLimits,
    pub(super) events: Option<Sender<Event>>,
    packet_bytes: usize,
    deadline: Option<std::time::Instant>,
    /// A synchronous reply waiting for the guest's matching `sync_read`.
    reply: Option<Vec<u8>>,
}

/// Refuse interpreter modules whose imports or exports differ from the private ABI.
pub(crate) fn check(module: &Module) -> std::result::Result<(), String> {
    for import in module.imports() {
        let (namespace, name) = (import.module(), import.name());
        let allowed = match import.ty() {
            ExternType::Memory(memory) => {
                (namespace, name) == (abi::MEMORY_MODULE, abi::MEMORY_IMPORT)
                    && memory.minimum() == u64::from(abi::MEMORY_INITIAL_PAGES)
                    && memory.maximum().is_none()
                    && !memory.is_64()
                    && !memory.is_shared()
            }
            ExternType::Func(_) => {
                (namespace == abi::IMPORT_MODULE
                    && [abi::SYNC_IMPORT, abi::SYNC_READ_IMPORT].contains(&name))
                    || (namespace == abi::WASI_MODULE && abi::WASI_IMPORTS.contains(&name))
            }
            _ => false,
        };
        if !allowed {
            return Err(format!(
                "interpreter import {namespace}::{name} is not allowed"
            ));
        }
    }
    for name in [abi::INITIALIZE_EXPORT, abi::ALLOC_EXPORT, abi::CALL_EXPORT] {
        if !matches!(module.get_export(name), Some(ExternType::Func(_))) {
            return Err(format!("interpreter export {name} is missing"));
        }
    }
    Ok(())
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

/// Serve one synchronous request while the guest stack waits; diagnostics need no scope.
fn synchronous(state: &State, packet: String) -> std::result::Result<String, String> {
    if let Some(result) = state.diagnostics.accept(&packet) {
        return result
            .map(|value| value.to_string())
            .map_err(|error| error.to_string());
    }
    let reply = request(state, packet, false)?;
    let remaining = state
        .deadline
        .map(|deadline| deadline.saturating_duration_since(std::time::Instant::now()))
        .unwrap_or_default();
    let reply = std::pin::pin!(reply);
    let deadline = std::pin::pin!(futures_timer::Delay::new(remaining));
    match futures::executor::block_on(futures::future::select(reply, deadline)) {
        futures::future::Either::Left((reply, _)) => {
            reply.unwrap_or_else(|_| Err("host callback scope ended".into()))
        }
        futures::future::Either::Right(_) => {
            Err("host callback reply exceeded its deadline".into())
        }
    }
}

/// Link the private host functions and the exact WASI allowlist.
fn linker(engine: &wasmtime::Engine) -> wasmtime::Result<Linker<State>> {
    let mut linker = Linker::<State>::new(engine);
    super::wasi::add(&mut linker)?;
    linker.func_wrap(
        abi::IMPORT_MODULE,
        abi::SYNC_IMPORT,
        |mut caller: Caller<'_, State>, pointer: u32, length: u32| -> wasmtime::Result<u32> {
            let memory = caller.data().memory.expect("memory is defined");
            if caller.data().reply.is_some() {
                return Err(wasmtime::Error::msg("synchronous reply was not read"));
            }
            let reply = if length as usize > caller.data().packet_bytes {
                Err("host packet limit exceeded".to_owned())
            } else {
                let start = pointer as usize;
                let packet = memory
                    .data(&caller)
                    .get(start..start + length as usize)
                    .ok_or_else(|| wasmtime::Error::msg("host packet is outside guest memory"))?;
                let packet = std::str::from_utf8(packet)
                    .map_err(|_| wasmtime::Error::msg("host packet is not UTF-8"))?
                    .to_owned();
                synchronous(caller.data(), packet)
            };
            let (flag, reply) = match reply {
                Ok(value) => (0, value),
                Err(message) => (abi::SYNC_ERROR, message),
            };
            let length = u32::try_from(reply.len())
                .ok()
                .filter(|length| *length <= abi::LENGTH_LIMIT)
                .ok_or_else(|| wasmtime::Error::msg("host reply exceeds the ABI length limit"))?;
            caller.data_mut().reply = Some(reply.into_bytes());
            Ok(flag | length)
        },
    )?;
    linker.func_wrap(
        abi::IMPORT_MODULE,
        abi::SYNC_READ_IMPORT,
        |mut caller: Caller<'_, State>, destination: u32, length: u32| -> wasmtime::Result<()> {
            let memory = caller.data().memory.expect("memory is defined");
            let reply = caller
                .data_mut()
                .reply
                .take()
                .filter(|reply| reply.len() == length as usize)
                .ok_or_else(|| wasmtime::Error::msg("synchronous reply read does not match"))?;
            memory.write(&mut caller, destination as usize, &reply)?;
            Ok(())
        },
    )?;
    Ok(linker)
}

/// Create one isolated Store whose imported memory and StoreLimits both enforce the memory bound.
pub(super) async fn instantiate(
    engine: &Engine,
    diagnostics: super::diagnostics::Diagnostics,
) -> wasmtime::Result<(Store<State>, super::steps::Driver)> {
    let module = engine
        .interpreter
        .as_ref()
        .ok_or_else(|| wasmtime::Error::msg("source Orbs require a configured interpreter"))?;
    let mut store = Store::new(
        &engine.engine,
        State {
            diagnostics,
            wasi: super::wasi::Wasi::default(),
            memory: None,
            limits: StoreLimitsBuilder::new()
                .memory_size(engine.limits.memory_bytes)
                .build(),
            events: None,
            packet_bytes: engine.limits.packet_bytes,
            deadline: None,
            reply: None,
        },
    );
    store.limiter(|state| &mut state.limits);
    store.set_fuel(engine.limits.fuel)?;
    store.fuel_async_yield_interval(Some(10_000))?;
    let maximum = u32::try_from(engine.limits.memory_bytes / abi::PAGE_BYTES as usize)?;
    let memory = Memory::new(
        &mut store,
        MemoryType::new(abi::MEMORY_INITIAL_PAGES, Some(maximum))?,
    )?;
    store.data_mut().memory = Some(memory);
    let mut linker = linker(&engine.engine)?;
    linker.define(&store, abi::MEMORY_MODULE, abi::MEMORY_IMPORT, memory)?;
    let instance = linker.instantiate_async(&mut store, module).await?;
    let driver = super::steps::Driver::new(&instance, &mut store, memory).await?;
    Ok((store, driver))
}

/// Instantiate once, run commands serially, and quarantine the Store after a trap.
pub(super) async fn run(
    engine: Engine,
    commands: mpsc::Receiver<Command>,
    poison: Arc<Mutex<Option<String>>>,
    diagnostics: super::diagnostics::Diagnostics,
) -> super::Terminal {
    let outcome = async {
        let (mut store, driver) = instantiate(&engine, diagnostics).await?;
        while let Ok(command) = commands.recv() {
            store.data_mut().events = Some(command.events.clone());
            store.data_mut().deadline = Some(
                std::time::Instant::now()
                    + std::time::Duration::from_millis(engine.limits.timeout_ms),
            );
            store.set_fuel(engine.limits.fuel)?;
            let result = tokio::time::timeout(
                std::time::Duration::from_millis(engine.limits.timeout_ms),
                driver.call(command.operation, command.packet, &mut store),
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
