//! Own the Wasmtime Store, its bounded imported memory and the interpreter's imports on one worker.
use super::{Command, Event};
use crate::{Engine, abi};
use async_channel::Sender;
use std::sync::{Arc, Mutex, mpsc};
use tokio::sync::oneshot;
use wasmtime::{
    AsContextMut, Caller, ExternType, Linker, Memory, MemoryType, Module, Store, StoreLimits,
    StoreLimitsBuilder,
};

pub(super) struct State {
    pub(super) diagnostics: super::diagnostics::Diagnostics,
    pub(super) wasi: super::wasi::Wasi,
    /// The instance's only linear memory, created by the host with the configured maximum.
    pub(super) memory: Option<Memory>,
    limits: StoreLimits,
    pub(super) events: Option<Sender<Event>>,
    pub(super) packet_bytes: usize,
    deadline: Option<std::time::Instant>,
    /// A synchronous reply waiting for the guest's matching `sync_read`.
    reply: Option<Vec<u8>>,
    /// Standard WASI context and resources of this revision; dropped with the Store.
    ctx: wasmtime_wasi::WasiCtx,
    table: wasmtime::component::ResourceTable,
    http: wasmtime_wasi_http::WasiHttpCtx,
    hooks: crate::http::Hooks,
    /// Physical HTTP work of this revision: active work (resolver, connect, closes, teardown)
    /// and revision-owned connection drivers.
    pub(super) transport: crate::http::Transport,
    pub(super) standard: super::standard::Standard,
    pub(super) callback_scope: crate::io::CallbackScope,
    /// Whether the provider bound to `wasi:sockets/ip-name-lookup` grants name resolution.
    pub(super) resolve: bool,
    /// The incoming HTTP request of the active ingress callback, if any.
    pub(super) ingress: Option<Ingress>,
    /// The host-created end of the active callback's whole byte-stream result, until
    /// `STREAM_RESULT` publishes it. Dropped unpublished at the callback's end, so its caller
    /// learns that no stream was returned.
    pub(super) stream: Option<super::StreamCall>,
}

/// The upstream request body's processing result.
pub(super) type Received = std::pin::Pin<
    Box<
        dyn std::future::Future<Output = std::result::Result<(), wasmtime_wasi_http::Error>> + Send,
    >,
>;

/// One source ingress callback: the call's exchange, its unpublished head and the request
/// handle issued to the guest. Its response is published once, by `INGRESS_HEAD`.
pub(super) struct Ingress {
    pub(super) exchange: Arc<crate::ingress::Exchange>,
    pub(super) head: Option<oneshot::Sender<crate::ingress::Head>>,
    /// The request body's processing result, joined into the response's transmission result.
    pub(super) received: Option<Received>,
    pub(super) request: u64,
    /// The handler's `error-code`, which completes the callback with an error (after
    /// settlement) for hosts that read it from the completion.
    pub(super) refused: Option<String>,
}

impl State {
    /// Start an ingress callback: the request becomes an owned standard handle of this
    /// revision, and the guest's dispatch packet names it as `request`.
    fn begin_ingress(
        &mut self,
        packet: &str,
        call: crate::ingress::IngressCall,
    ) -> wasmtime::Result<std::result::Result<String, String>> {
        let mut packet: serde_json::Value = match serde_json::from_str(packet) {
            Ok(serde_json::Value::Object(packet))
                if packet.get("kind") == Some(&"ingress".into()) =>
            {
                serde_json::Value::Object(packet)
            }
            _ => return Ok(Err("malformed ingress command".into())),
        };
        let crate::ingress::IngressCall {
            request,
            exchange,
            head,
        } = call;
        let (request, received) =
            wasmtime_wasi_http::p3::Request::from_http(&mut self.hooks, request);
        let request = self.table.push(request)?;
        let id = match super::standard::issue_request(self, request)? {
            Ok(id) => id,
            Err(refusal) => return Ok(Err(refusal)),
        };
        packet["request"] = id.into();
        self.ingress = Some(Ingress {
            exchange,
            head: Some(head),
            received: Some(Box::pin(received)),
            request: id,
            refused: None,
        });
        Ok(Ok(packet.to_string()))
    }

    /// End an ingress callback. A body still being delivered after a failed or unsettled
    /// callback ends with an error, never a truncated end; a request the guest never took is
    /// released, since a request belongs to its callback.
    ///
    /// Returns the error the callback completes with: the handler's `error-code`, or a
    /// callback that ended without ever publishing its response. The caller applies it only
    /// to a callback that otherwise succeeded, so a trap or the callback's own error wins.
    fn end_ingress(&mut self, failed: bool) -> wasmtime::Result<Option<String>> {
        let Some(ingress) = self.ingress.take() else {
            return Ok(None);
        };
        // Decided before cleanup: an unpublished head is a failed request, never `ok` without
        // a response.
        let refused = match (ingress.refused, ingress.head.is_some()) {
            (Some(refused), _) => Some(refused),
            (None, true) => Some("the incoming HTTP handler returned no response".to_owned()),
            (None, false) => None,
        };
        if failed || !ingress.exchange.ended() {
            ingress.exchange.fail();
        }
        super::standard::release_unclaimed(self, ingress.request)?;
        Ok(refused)
    }
}

impl crate::sockets::CallCancellation for State {
    /// Source callbacks cancel by aborting their Store tasks, which drops a pending connect or
    /// UDP operation; there is no call signal to race.
    fn cancellation(&mut self) -> Option<crate::http::Cancelled> {
        None
    }
}

impl crate::dns::DnsView for State {
    fn dns(&mut self) -> crate::dns::Lookup<'_> {
        crate::dns::Lookup {
            allowed: self.resolve,
            transport: &self.transport,
        }
    }
}

impl wasmtime_wasi::WasiView for State {
    fn ctx(&mut self) -> wasmtime_wasi::WasiCtxView<'_> {
        wasmtime_wasi::WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

impl wasmtime_wasi_http::WasiHttpView for State {
    /// Standard HTTP resources share the Store's table; sends pass through Tembra hooks.
    fn http(&mut self) -> wasmtime_wasi_http::WasiHttpCtxView<'_> {
        wasmtime_wasi_http::WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: &mut self.hooks,
        }
    }
}

#[cfg(test)]
impl State {
    /// Replace this test Store's HTTP grants and transport.
    pub(super) fn grant_http(&mut self, grants: crate::Grants, transport: crate::http::Transport) {
        self.hooks = crate::http::Hooks::new(
            Arc::new(grants),
            Arc::new(CallbackAuthority(self.callback_scope.clone())),
            transport.clone(),
        );
        self.transport = transport;
    }
}

/// Source HTTP authority: the guest callback that is active when upstream sends.
struct CallbackAuthority(crate::io::CallbackScope);

impl crate::http::Authority for CallbackAuthority {
    fn admit(&self) -> Result<crate::http::Admission, crate::http::Refusal> {
        if !self.0.active() {
            return Err(crate::http::Refusal::Ended("callback ended".into()));
        }
        // Source sends are callback-owned Store tasks: the Store ledger is their owner.
        Ok(crate::http::Admission {
            confirmation: Box::pin(std::future::ready(Ok(()))),
            call: None,
            // Source sends are aborted with their callback's Store tasks instead.
            cancelled: None,
        })
    }
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
    // The one synchronous import is async-capable: almost every call is ready in its first
    // poll, while WIT-synchronous calls whose upstream is Rust-`async` keep the guest suspended
    // on its fiber until they finish. Same import name and signature as before.
    linker.func_wrap_async(
        abi::IMPORT_MODULE,
        abi::SYNC_IMPORT,
        |mut caller: Caller<'_, State>, (pointer, length): (u32, u32)| {
            Box::new(async move {
                let memory = caller.data().memory.expect("memory is defined");
                if caller.data().reply.is_some() {
                    return Err(wasmtime::Error::msg("synchronous reply was not read"));
                }
                let reply = if length as usize > caller.data().packet_bytes {
                    Err("host packet limit exceeded".to_owned())
                } else if length > 0
                    && memory.data(&caller).get(pointer as usize).copied()
                        == Some(super::standard::PACKET_TAG)
                {
                    // Generated standard WASI calls run in this Store without a foreign carrier.
                    let start = pointer as usize;
                    let packet = memory
                        .data(&caller)
                        .get(start..start + length as usize)
                        .ok_or_else(|| wasmtime::Error::msg("host packet is outside guest memory"))?
                        .to_vec();
                    if caller.data().events.is_none() {
                        Err("no active invocation".to_owned())
                    } else {
                        super::standard::synchronous(caller.as_context_mut(), &packet).await?
                    }
                } else {
                    let start = pointer as usize;
                    let packet = memory
                        .data(&caller)
                        .get(start..start + length as usize)
                        .ok_or_else(|| {
                            wasmtime::Error::msg("host packet is outside guest memory")
                        })?;
                    let packet = std::str::from_utf8(packet)
                        .map_err(|_| wasmtime::Error::msg("host packet is not UTF-8"))?
                        .to_owned();
                    synchronous(caller.data(), packet).map(String::into_bytes)
                };
                let (flag, reply) = match reply {
                    Ok(value) => (0, value),
                    Err(message) => (abi::SYNC_ERROR, message.into_bytes()),
                };
                let length = u32::try_from(reply.len())
                    .ok()
                    .filter(|length| *length <= abi::LENGTH_LIMIT)
                    .ok_or_else(|| {
                        wasmtime::Error::msg("host reply exceeds the ABI length limit")
                    })?;
                caller.data_mut().reply = Some(reply);
                Ok(flag | length)
            })
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
    standard: crate::io::StandardContext,
    transport: crate::http::Transport,
) -> wasmtime::Result<(Store<State>, super::steps::Driver)> {
    let crate::io::StandardContext {
        mut wasi,
        allowed,
        scope,
        clocks,
        grants,
        resolve,
    } = standard;
    // Qualified source filesystem calls run inline during bounded Store polls. They cannot
    // leave detached spawn_blocking work behind when a callback future is revoked.
    wasi.allow_blocking_current_thread(true);
    let module = engine
        .interpreter
        .as_ref()
        .ok_or_else(|| wasmtime::Error::msg("source Orbs require a configured interpreter"))?;
    let mut store = Store::new(
        &engine.engine,
        State {
            diagnostics,
            wasi: super::wasi::Wasi::new(clocks),
            memory: None,
            limits: StoreLimitsBuilder::new()
                .memory_size(engine.limits.memory_bytes)
                .build(),
            events: None,
            packet_bytes: engine.limits.packet_bytes,
            deadline: None,
            reply: None,
            ctx: wasi.build(),
            table: wasmtime::component::ResourceTable::new(),
            http: wasmtime_wasi_http::WasiHttpCtx::new(),
            hooks: crate::http::Hooks::new(
                grants,
                Arc::new(CallbackAuthority(scope.clone())),
                transport.clone(),
            ),
            transport,
            standard: super::standard::Standard::new(
                allowed,
                engine.open_files.clone(),
                Arc::clone(&engine.stream_adapter),
            ),
            callback_scope: scope,
            resolve,
            ingress: None,
            stream: None,
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
    standard: crate::io::StandardContext,
) -> super::Terminal {
    let transport = crate::http::Transport::default();
    let outcome = async {
        let (mut store, driver) =
            instantiate(&engine, diagnostics, standard, transport.clone()).await?;
        while let Ok(mut command) = commands.recv() {
            // An ingress command issues its request before the guest runs.
            let mut packet = std::mem::take(&mut command.packet);
            if let Some(call) = command.ingress.take() {
                match store.data_mut().begin_ingress(&packet, call)? {
                    Ok(named) => packet = named,
                    Err(refusal) => {
                        let _ = command.events.send(Event::Done(Err(refusal))).await;
                        continue;
                    }
                }
            }
            store.data_mut().stream = command.stream.take();
            store.data_mut().events = Some(command.events.clone());
            store.data_mut().deadline = Some(
                std::time::Instant::now()
                    + std::time::Duration::from_millis(engine.limits.timeout_ms),
            );
            store.set_fuel(engine.limits.fuel)?;
            let result = tokio::time::timeout(
                std::time::Duration::from_millis(engine.limits.timeout_ms),
                driver.call(command.operation, packet, &mut store),
            )
            .await
            .map_err(|_| {
                wasmtime::Error::msg(
                    "interpreter callback exceeded its wall time limit; Store quarantined",
                )
            })
            .and_then(|result| result)
            .map_err(|error| super::standard::attribute_trap(error, engine.interpreter.as_deref()));
            store.data_mut().events = None;
            store.data_mut().stream = None;
            let mut trapped = result.as_ref().map_or(true, |outcome| outcome.interrupted);
            let mut result = result;
            // Ending an ingress callback cannot fail unless the Store's tables are inconsistent;
            // that quarantines it like a trap.
            match store.data_mut().end_ingress(trapped) {
                // A handler `error-code` completes the callback with that error.
                Ok(Some(refused)) => {
                    result = result.map(|outcome| super::steps::Outcome {
                        value: outcome.value.and(Err(refused)),
                        interrupted: outcome.interrupted,
                    })
                }
                Ok(None) => {}
                Err(error) => {
                    trapped = true;
                    result = Err(error);
                }
            }
            let result = result
                .map(|outcome| outcome.value)
                .unwrap_or_else(|error| {
                    Err(match crate::providers::exit_status(&error) {
                        Some(status) => {
                            format!("orb exited with status {status}; revision retired")
                        }
                        None => format!("interpreter trap: {error:#}"),
                    })
                })
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
    // The Store is gone: upstream's connection driver tasks are aborted and drop on the next
    // runtime turn, closing their sockets; resolver jobs end when their lookups return. The
    // revision retires only after every unit physically ended.
    transport.store_work.settled().await;
    transport.drivers.settled().await;
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

/// Supply a real upstream context to focused binding tests without a guest module.
#[cfg(test)]
pub(super) fn test_state(ctx: wasmtime_wasi::WasiCtx) -> State {
    let transport = crate::http::Transport::default();
    let scope = crate::io::CallbackScope::default();
    scope.begin_callback();
    State {
        diagnostics: super::diagnostics::Diagnostics {
            orb: "standard-test".into(),
            sink: None,
        },
        wasi: super::wasi::Wasi::default(),
        memory: None,
        limits: StoreLimitsBuilder::new().build(),
        events: None,
        packet_bytes: 1 << 20,
        deadline: None,
        reply: None,
        ctx,
        table: Default::default(),
        http: wasmtime_wasi_http::WasiHttpCtx::new(),
        hooks: crate::http::Hooks::new(
            Arc::default(),
            Arc::new(CallbackAuthority(scope.clone())),
            transport.clone(),
        ),
        transport,
        standard: super::standard::Standard::new(
            Default::default(),
            crate::engine::FileBudget::new(crate::engine::OPEN_FILES),
            Arc::default(),
        ),
        callback_scope: scope,
        resolve: false,
        ingress: None,
        stream: None,
    }
}
