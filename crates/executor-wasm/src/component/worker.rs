//! Run one compiled component Store behind the shared session owner and private host protocol.
//!
//! The Store never receives a kernel object, Context, Reference or callback. Declarations
//! acquire dependency references through the existing synchronous `reference.declare` request;
//! every dependency import issues the existing asynchronous `reference.call` request on the
//! channel of the call that physically issued it. The embedding adapter performs that call under
//! its admitted invocation, so kernel execution frames and WIT slot validation apply unchanged.
//!
//! Authority is attributed to Wasmtime task groups, not to a mutable "current call" field. A
//! call is staged with `start_call_concurrent`; the task-group hook binds that call's authority
//! when its group starts. An import resolves the group entered by the thread that issued it and
//! is refused when that group carries no authority (initialization or an unrelated task). A call
//! completes only after its result is available and its task group has finished, so task work
//! that outlives the returned value still runs under the call's own authority and can never
//! observe a later call's scope.
//!
//! Standard `wasi:http` imports use upstream `wasmtime-wasi-http` P3 resources and its sender.
//! Each request is attributed the same way as a dependency import: the hook captures the entered
//! task group's authority synchronously, checks the installation's HTTP origin grants and then
//! asks the originating call, through its own channel, whether its context is still admitted.
use super::values;
use crate::{
    Engine,
    abi::Operation,
    worker::{Command, Event, Terminal},
};
use async_channel::Sender;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex, mpsc},
};
use tokio::sync::oneshot;
use wasmtime::{
    Store, StoreLimits, StoreLimitsBuilder,
    component::{
        Accessor, Component, Func, Linker, Resource, ResourceTable, TaskGroupHook, TaskGroupId,
        TypedFunc, Val, types::ComponentFunc,
    },
};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_wasi_http::p3::{self, bindings::http::types::ErrorCode};
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpView};

/// Default dependency object selected by a plain WIT import, which carries no object key.
const DEFAULT_KEY: &str = "main";

/// Exported callbacks and selected Orb imports, resolved from the prepared contract.
pub(crate) struct Layout {
    /// JavaScript service alias to its interface identity and method spellings.
    pub exports: BTreeMap<String, Export>,
    /// Orb-bound import interfaces, in world order.
    pub imports: Vec<Import>,
    /// Checked contribution binding to its callback export; never an ordinary service.
    pub contributions: BTreeMap<String, (String, String)>,
    /// The exact incoming HTTP handler export, served only through the ingress adapter.
    pub ingress: Option<String>,
}

/// The standard handler's `handle`, typed against the host's own request and response
/// resources. Typing it at instantiation checks the actual binary's signature.
type Handler = TypedFunc<(Resource<p3::Request>,), (Result<Resource<p3::Response>, ErrorCode>,)>;

/// Callable exports resolved once per Store.
struct Exports {
    functions: BTreeMap<(String, String), Func>,
    handler: Option<Handler>,
}

pub(crate) struct Export {
    pub interface: String,
    /// JavaScript method spelling to original WIT function name.
    pub methods: BTreeMap<String, String>,
}

pub(crate) struct Import {
    /// World import key; the embedding adapter maps it to the selected provider service.
    pub binding: String,
    pub functions: Vec<ImportFunction>,
    /// This import anchors a contribution's type and grants no callable service reference.
    pub contribution: bool,
}

pub(crate) struct ImportFunction {
    pub name: String,
    /// JavaScript spelling used by the private `reference.call` request.
    pub spelling: String,
    /// `async func` imports are linked as concurrent host functions.
    pub asynchronous: bool,
}

/// Immutable authority of one admitted call, shared by every thread of its task group.
#[derive(Clone)]
struct Authority {
    events: Sender<Event>,
    /// Orb root environment whose handle table holds the dependency references.
    scope: u64,
    handles: Arc<BTreeMap<String, u32>>,
}

struct Bound {
    authority: Authority,
    finished: Option<futures::channel::oneshot::Sender<()>>,
    /// Physical HTTP work of this call (resolver, connect, closes and teardown of bodies it
    /// reads or drops); the call completes only after it ends. Idle retained response drivers
    /// are never counted here.
    call_work: crate::http::Ledger,
    /// Resolves when the call's callback epoch ends (cancellation, or completion).
    cancelled: crate::http::Cancelled,
}

/// Task-group attribution shared between the Store's hook and its import functions.
#[derive(Default)]
struct Groups {
    /// Group of the thread the runtime has entered, if any.
    current: Option<TaskGroupId>,
    /// Authority waiting for the next staged call's group to start.
    pending: Option<Bound>,
    bound: HashMap<TaskGroupId, Bound>,
    /// The session's callback epochs; the caller's cancellation ends the call's epoch.
    scope: crate::io::CallbackScope,
}

struct Hook(Arc<Mutex<Groups>>);

impl Hook {
    fn groups(&self) -> wasmtime::Result<std::sync::MutexGuard<'_, Groups>> {
        self.0
            .lock()
            .map_err(|_| wasmtime::Error::msg("task-group attribution is poisoned"))
    }
}

impl TaskGroupHook for Hook {
    /// Bind the staged call's authority; groups started without one never receive authority.
    fn handle_start(&mut self, id: TaskGroupId) -> wasmtime::Result<()> {
        let mut groups = self.groups()?;
        if let Some(pending) = groups.pending.take() {
            groups.bound.insert(id, pending);
        }
        Ok(())
    }

    fn handle_enter(&mut self, id: TaskGroupId) -> wasmtime::Result<()> {
        self.groups()?.current = Some(id);
        Ok(())
    }

    fn handle_exit(&mut self, id: TaskGroupId) -> wasmtime::Result<()> {
        let mut groups = self.groups()?;
        if groups.current == Some(id) {
            groups.current = None;
        }
        Ok(())
    }

    /// Retire the authority before the identifier can be reused, then release the waiting call.
    fn handle_finish(&mut self, id: TaskGroupId) -> wasmtime::Result<()> {
        let bound = self.groups()?.bound.remove(&id);
        if let Some(mut bound) = bound
            && let Some(finished) = bound.finished.take()
        {
            let _ = finished.send(());
        }
        Ok(())
    }
}

pub(crate) struct State {
    wasi: WasiCtx,
    http: WasiHttpCtx,
    hooks: crate::http::Hooks,
    /// The same transport the hooks use; calls mark its command boundary.
    transport: crate::http::Transport,
    table: ResourceTable,
    limits: StoreLimits,
    groups: Arc<Mutex<Groups>>,
    packet_bytes: usize,
    /// Whether the provider bound to `wasi:sockets/ip-name-lookup` grants name resolution.
    resolve: bool,
    /// The private stream adapter of socket output streams; instantiated only for components
    /// that import `wasi:sockets/types`.
    adapter: crate::worker::stream_adapter::Adapter<State>,
}

impl crate::worker::stream_adapter::AdapterStore for State {
    type Permit = ();

    const GUEST_STREAMS: bool = true;

    /// Socket streams and sends belong to the call that uses them, at the same boundary as HTTP
    /// bodies.
    fn commands(&self) -> &crate::http::Commands {
        &self.transport.commands
    }

    fn adapter(&mut self) -> &mut crate::worker::stream_adapter::Adapter<Self> {
        &mut self.adapter
    }

    fn linked(
        compiled: &crate::worker::StreamAdapter,
    ) -> &std::sync::OnceLock<Result<wasmtime::component::InstancePre<Self>, String>> {
        &compiled.compiled
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

impl crate::sockets::CallCancellation for State {
    /// The cancellation of the call whose task group is running, exactly as HTTP admits it.
    fn cancellation(&mut self) -> Option<crate::http::Cancelled> {
        crate::http::Authority::admit(&GroupAuthority(self.groups.clone()))
            .ok()
            .and_then(|admission| admission.cancelled)
    }
}

/// Link upstream WASI interface by interface, with name lookup served by Tembra's tracked
/// resolver instead of upstream's untracked lookup jobs, and socket waits released by the
/// caller's cancellation.
fn link_wasi(linker: &mut Linker<State>) -> wasmtime::Result<()> {
    use wasmtime_wasi::p3;
    p3::clocks::add_to_linker(linker)?;
    p3::random::add_to_linker(linker)?;
    p3::cli::add_to_linker(linker)?;
    p3::filesystem::add_to_linker(linker)?;
    // Upstream sockets, with pending operations released by the caller's cancellation.
    p3::bindings::sockets::types::add_to_linker::<State, crate::sockets::CallSockets>(
        linker,
        <State as wasmtime_wasi::sockets::WasiSocketsView>::sockets,
    )?;
    p3::bindings::sockets::ip_name_lookup::add_to_linker::<State, crate::dns::Dns>(
        linker,
        <State as crate::dns::DnsView>::dns,
    )
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

impl WasiHttpView for State {
    /// Standard HTTP resources share the Store's table; sends pass through the shared hooks.
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: &mut self.hooks,
        }
    }
}

/// Compiled HTTP authority: the admitted invocation whose task group is running.
struct GroupAuthority(Arc<Mutex<Groups>>);

impl crate::http::Authority for GroupAuthority {
    /// Resolve the originating call's channel synchronously inside the guest call; the
    /// returned confirmation asks that call's adapter whether its context is still admitted.
    fn admit(&self) -> Result<crate::http::Admission, crate::http::Refusal> {
        use crate::http::Refusal;
        let (events, call_work, cancelled) = {
            let groups = self.0.lock().map_err(|_| Refusal::Unattributed)?;
            groups
                .current
                .and_then(|group| groups.bound.get(&group))
                .map(|bound| {
                    (
                        bound.authority.events.clone(),
                        bound.call_work.clone(),
                        bound.cancelled.clone(),
                    )
                })
                .ok_or(Refusal::Unattributed)?
        };
        Ok(crate::http::Admission {
            // The adapter refusing (or gone) means the call was cancelled or has ended.
            confirmation: Box::pin(
                async move { check_context(&events).await.map_err(Refusal::Ended) },
            ),
            call: Some(call_work),
            cancelled: Some(cancelled),
        })
    }
}

/// Ask the originating call's adapter to confirm its context is still admitted.
async fn check_context(events: &Sender<Event>) -> Result<(), String> {
    let (reply, receive) = oneshot::channel();
    events
        .try_send(Event::Host {
            packet: json!({"operation": "context.check"}).to_string(),
            asynchronous: false,
            reply,
        })
        .map_err(|_| "the originating call has ended".to_owned())?;
    receive
        .await
        .map_err(|_| "the originating call has ended".to_owned())?
        .map(|_| ())
}

/// Host-selected commands; a component has no guest closures to validate or call back.
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum Packet {
    Create {
        environment: u64,
        service: Option<String>,
    },
    Invoke {
        environment: u64,
        service: Option<String>,
        method: String,
        args: Value,
    },
    Contribute {
        environment: u64,
        binding: String,
        service: String,
        method: String,
        args: Value,
    },
    Release {
        environment: u64,
    },
    Ingress {
        environment: u64,
    },
    #[serde(other)]
    Unsupported,
}

/// Dependency handles issued to Orb root environments, and each service's root.
#[derive(Default)]
struct Environments {
    roots: BTreeMap<u64, Arc<BTreeMap<String, u32>>>,
    parents: BTreeMap<u64, u64>,
}

/// Instantiate the component, report readiness, then serve commands until the owner closes or a
/// call fails terminally.
pub(crate) async fn run(
    engine: Engine,
    component: Component,
    wasi: (WasiCtxBuilder, bool),
    grants: Arc<crate::Grants>,
    layout: Layout,
    ready: mpsc::Sender<std::result::Result<(), String>>,
    // The session's callback epochs travel with its command queue.
    calls: (mpsc::Receiver<Command>, crate::io::CallbackScope),
) -> Terminal {
    let transport = crate::http::Transport::default();
    let terminal = live(
        (engine, component, wasi, grants, layout),
        ready,
        calls,
        &transport,
    )
    .await;
    // The Store is gone: aborted connection drivers drop on the next runtime turn and close
    // their sockets; resolver jobs end when their lookups return. Only then is the revision
    // retired.
    transport.store_work.settled().await;
    transport.drivers.settled().await;
    terminal
}

/// Serve commands with one Store; the Store is dropped when this returns.
async fn live(
    (engine, component, wasi, grants, layout): (
        Engine,
        Component,
        (WasiCtxBuilder, bool),
        Arc<crate::Grants>,
        Layout,
    ),
    ready: mpsc::Sender<std::result::Result<(), String>>,
    (commands, scope): (mpsc::Receiver<Command>, crate::io::CallbackScope),
    transport: &crate::http::Transport,
) -> Terminal {
    let groups = Arc::new(Mutex::new(Groups {
        scope,
        ..Groups::default()
    }));
    let instance = instantiate(
        &engine, &component, wasi, grants, &layout, &groups, transport,
    )
    .await;
    let (mut store, exports) = match instance {
        Ok(instance) => instance,
        Err(error) => {
            let _ = ready.send(Err(format!("component initialization failed: {error:#}")));
            return Vec::new();
        }
    };
    let _ = ready.send(Ok(()));
    let mut environments = Environments::default();
    while let Ok(mut command) = commands.recv() {
        let served = serve(
            &engine,
            &mut store,
            &exports,
            &layout,
            &mut environments,
            &groups,
            &mut command,
        )
        .await;
        match served {
            Ok(result) => {
                let _ = command.events.send(Event::Done(result)).await;
            }
            Err(error) => {
                // The Store is dropped with this future; its completion waits for runtime shutdown.
                let message = match crate::providers::exit_status(&error) {
                    Some(status) => format!("orb exited with status {status}; component retired"),
                    None => format!("component trap: {error:#}"),
                };
                return vec![(command.events, message)];
            }
        }
    }
    Vec::new()
}

/// Link granted WASI and the selected Orb imports, then instantiate within the callback bound.
async fn instantiate(
    engine: &Engine,
    component: &Component,
    (mut wasi, resolve): (WasiCtxBuilder, bool),
    grants: Arc<crate::Grants>,
    layout: &Layout,
    groups: &Arc<Mutex<Groups>>,
    transport: &crate::http::Transport,
) -> wasmtime::Result<(Store<State>, Exports)> {
    let mut linker = Linker::<State>::new(&engine.engine);
    link_wasi(&mut linker)?;
    // Linking grants nothing: installation requires an HTTP grant for each wasi:http import and
    // every request is checked against its granted origins.
    wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;
    link_imports(&mut linker, layout)?;
    let mut store = Store::new(
        &engine.engine,
        State {
            wasi: wasi.build(),
            http: WasiHttpCtx::new(),
            hooks: crate::http::Hooks::new(
                grants,
                Arc::new(GroupAuthority(groups.clone())),
                transport.clone(),
            ),
            transport: transport.clone(),
            table: ResourceTable::new(),
            limits: StoreLimitsBuilder::new()
                .memory_size(engine.limits.memory_bytes)
                .build(),
            groups: groups.clone(),
            packet_bytes: engine.limits.packet_bytes,
            resolve,
            adapter: crate::worker::stream_adapter::Adapter::new(Arc::clone(
                &engine.stream_adapter,
            )),
        },
    );
    store.limiter(|state| &mut state.limits);
    store.set_fuel(engine.limits.fuel)?;
    store.fuel_async_yield_interval(Some(10_000))?;
    store.task_group_hook(Hook(groups.clone()));
    // Components that import sockets get the stream adapter before they are instantiated, so
    // socket streams opened or sent while the component initializes take the same route.
    let sockets = component
        .component_type()
        .imports(&engine.engine)
        .any(|(name, _)| name.starts_with("wasi:sockets/types@"));
    if sockets {
        crate::worker::stream_adapter::instantiate(&mut store).await?;
    }
    let instance = tokio::time::timeout(
        std::time::Duration::from_millis(engine.limits.timeout_ms),
        linker.instantiate_async(&mut store, component),
    )
    .await
    .map_err(|_| wasmtime::Error::msg("component initialization timed out"))??;
    let mut exports = BTreeMap::new();
    for (alias, export) in &layout.exports {
        let (_, interface) = instance
            .get_export(&mut store, None, &export.interface)
            .ok_or_else(|| wasmtime::Error::msg("missing component interface"))?;
        for (spelling, method) in &export.methods {
            let (_, index) = instance
                .get_export(&mut store, Some(&interface), method)
                .ok_or_else(|| wasmtime::Error::msg("missing component method"))?;
            let function = instance
                .get_func(&mut store, index)
                .ok_or_else(|| wasmtime::Error::msg("component export is not a function"))?;
            exports.insert((alias.clone(), spelling.clone()), function);
        }
    }
    // The handler's actual binary type must be exactly the standard one; anything else refuses
    // the installation here rather than failing a later request.
    let handler = match &layout.ingress {
        Some(handler) => {
            let (_, interface) = instance
                .get_export(&mut store, None, handler)
                .ok_or_else(|| wasmtime::Error::msg("missing incoming HTTP handler"))?;
            let (_, index) = instance
                .get_export(&mut store, Some(&interface), crate::ingress::HANDLE)
                .ok_or_else(|| wasmtime::Error::msg("missing incoming HTTP handle function"))?;
            Some(
                instance
                    .get_typed_func(&mut store, index)
                    .map_err(|error| {
                        wasmtime::Error::msg(format!(
                            "{handler}#handle differs from the standard signature: {error:#}"
                        ))
                    })?,
            )
        }
        None => None,
    };
    Ok((
        store,
        Exports {
            functions: exports,
            handler,
        },
    ))
}

/// Define each selected dependency function; WASI imports remain the installation's grants.
fn link_imports(linker: &mut Linker<State>, layout: &Layout) -> wasmtime::Result<()> {
    for import in &layout.imports {
        let mut instance = linker.instance(&import.binding)?;
        for function in &import.functions {
            if import.contribution {
                let message = format!(
                    "hook import {} is a contribution type anchor and cannot be called",
                    import.binding
                );
                instance.func_new(&function.name, move |_, _, _, _| {
                    Err(wasmtime::Error::msg(message.clone()))
                })?;
                continue;
            }
            let target = Arc::new((import.binding.clone(), function.spelling.clone()));
            if function.asynchronous {
                instance.func_new_concurrent(
                    &function.name,
                    move |accessor: &Accessor<State>, ty, params, results| {
                        // The first poll runs inside the issuing guest call, so the entered group
                        // is still the caller's when its authority is captured.
                        let request =
                            accessor.with(|mut access| request(access.data_mut(), &target, params));
                        Box::pin(async move { settle(request?, ty, results).await })
                    },
                )?;
            } else {
                instance.func_new_async(&function.name, move |store, ty, params, results| {
                    let request = request(store.data(), &target, params);
                    Box::new(async move { settle(request?, ty, results).await })
                })?;
            }
        }
    }
    Ok(())
}

/// Capture the issuing task group's authority and publish one bounded `reference.call` request.
fn request(
    state: &State,
    (binding, method): &(String, String),
    params: &[Val],
) -> wasmtime::Result<oneshot::Receiver<std::result::Result<String, String>>> {
    let authority = {
        let groups = state
            .groups
            .lock()
            .map_err(|_| wasmtime::Error::msg("task-group attribution is poisoned"))?;
        groups
            .current
            .and_then(|group| groups.bound.get(&group))
            .map(|bound| bound.authority.clone())
            .ok_or_else(|| wasmtime::Error::msg("dependency import has no admitted invocation"))?
    };
    let id = *authority
        .handles
        .get(binding)
        .ok_or_else(|| wasmtime::Error::msg("dependency import has no declared reference"))?;
    let args = params
        .iter()
        .cloned()
        .map(values::lift)
        .collect::<tembra::Result<Vec<_>>>()
        .map_err(|error| wasmtime::Error::msg(error.to_string()))?;
    let packet = json!({
        "operation": "reference.call",
        "scope": authority.scope,
        "id": id,
        "method": method,
        "args": args,
    })
    .to_string();
    if packet.len() > state.packet_bytes {
        return Err(wasmtime::Error::msg("host packet limit exceeded"));
    }
    let (reply, receive) = oneshot::channel();
    authority
        .events
        .try_send(Event::Host {
            packet,
            asynchronous: true,
            reply,
        })
        .map_err(|_| wasmtime::Error::msg("host operation queue is full or closed"))?;
    Ok(receive)
}

/// Lower a checked dependency result; a refusal traps the importer, which quarantines its Store.
async fn settle(
    receive: oneshot::Receiver<std::result::Result<String, String>>,
    ty: ComponentFunc,
    results: &mut [Val],
) -> wasmtime::Result<()> {
    let text = receive
        .await
        .map_err(|_| wasmtime::Error::msg("host callback scope ended"))?
        .map_err(|message| wasmtime::Error::msg(format!("dependency call failed: {message}")))?;
    let value: Value = serde_json::from_str(&text)?;
    match (ty.results().next(), results.first_mut()) {
        (Some(ty), Some(result)) => {
            *result = values::lower(ty, &value)
                .map_err(|error| wasmtime::Error::msg(error.to_string()))?;
        }
        (None, None) if value.is_null() => {}
        _ => {
            return Err(wasmtime::Error::msg(
                "dependency result differs from its import",
            ));
        }
    }
    Ok(())
}

/// Send one synchronous declaration request through the command's own channel.
async fn declaration(
    events: &Sender<Event>,
    packet: Value,
    limit: usize,
) -> std::result::Result<Value, String> {
    let packet = packet.to_string();
    if packet.len() > limit {
        return Err("host packet limit exceeded".into());
    }
    let (reply, receive) = oneshot::channel();
    events
        .send(Event::Host {
            packet,
            asynchronous: false,
            reply,
        })
        .await
        .map_err(|_| "declaration scope ended".to_owned())?;
    let text = receive
        .await
        .map_err(|_| "declaration scope ended".to_owned())??;
    serde_json::from_str(&text).map_err(|error| error.to_string())
}

/// Serve one command. The outer error is terminal; the inner error refuses only this command.
async fn serve(
    engine: &Engine,
    store: &mut Store<State>,
    exports: &Exports,
    layout: &Layout,
    environments: &mut Environments,
    groups: &Arc<Mutex<Groups>>,
    command: &mut Command,
) -> wasmtime::Result<std::result::Result<String, String>> {
    if !matches!(command.operation, Operation::Declare | Operation::Start) {
        return Ok(Err(
            "compiled components accept only declarations and calls".into(),
        ));
    }
    let packet: Packet = match serde_json::from_str(&command.packet) {
        Ok(packet) => packet,
        Err(error) => return Ok(Err(format!("malformed component command: {error}"))),
    };
    let expected = match packet {
        Packet::Create { .. } => Operation::Declare,
        Packet::Invoke { .. }
        | Packet::Contribute { .. }
        | Packet::Release { .. }
        | Packet::Ingress { .. } => Operation::Start,
        Packet::Unsupported => return Ok(Err("unknown compiled command".into())),
    };
    if command.operation != expected {
        return Ok(Err("command does not match its execution entrypoint".into()));
    }
    let limit = engine.limits.packet_bytes;
    let packet = match packet {
        Packet::Contribute {
            environment,
            binding,
            service,
            method,
            args,
        } => {
            if layout.contributions.get(&binding) != Some(&(service.clone(), method.clone())) {
                return Ok(Err("unknown compiled contribution callback".into()));
            }
            Packet::Invoke {
                environment,
                service: Some(service),
                method,
                args,
            }
        }
        packet => packet,
    };
    match packet {
        Packet::Create {
            environment,
            service,
        } => Ok(declare(
            environment,
            service,
            layout,
            environments,
            &command.events,
            limit,
        )
        .await
        .map(|()| "null".to_owned())),
        Packet::Release { environment } => {
            environments.roots.remove(&environment);
            environments.parents.remove(&environment);
            Ok(Ok("null".into()))
        }
        Packet::Invoke {
            environment,
            service: Some(service),
            method,
            args,
        } if command.operation == Operation::Start && command.ingress.is_none() => {
            let Some(function) = exports.functions.get(&(service, method)).copied() else {
                return Ok(Err("unknown component export".into()));
            };
            let authority = match authority(environment, layout, environments, command) {
                Ok(authority) => authority,
                Err(error) => return Ok(Err(error)),
            };
            invoke(engine, store, function, &args, authority, groups).await
        }
        Packet::Ingress { environment, .. } => {
            // Without its typed payload (or handler) there is nothing to serve; dropping the
            // payload closes its head channel, which the facade reports.
            let (Some(handler), Some(call)) = (exports.handler, command.ingress.take()) else {
                return Ok(Err("this revision serves no incoming HTTP request".into()));
            };
            let authority = match authority(environment, layout, environments, command) {
                Ok(authority) => authority,
                Err(error) => return Ok(Err(error)),
            };
            ingress(engine, store, handler, call, authority, groups).await
        }
        Packet::Invoke { .. } | Packet::Contribute { .. } | Packet::Unsupported => Ok(Err(
            "compiled components have no guest callbacks of this kind".into(),
        )),
    }
}

/// The admitted call's authority: its own event channel and its Orb root's dependency handles.
fn authority(
    environment: u64,
    layout: &Layout,
    environments: &Environments,
    command: &Command,
) -> std::result::Result<Authority, String> {
    let root = environments
        .parents
        .get(&environment)
        .copied()
        .unwrap_or(environment);
    let handles = match (environments.roots.get(&root), layout.imports.is_empty()) {
        (Some(handles), _) => handles.clone(),
        (None, true) => Arc::default(),
        (None, false) => return Err("component environment is not declared".into()),
    };
    Ok(Authority {
        events: command.events.clone(),
        scope: root,
        handles,
    })
}

/// Acquire default-key dependency references for an Orb root; services record their root.
async fn declare(
    environment: u64,
    service: Option<String>,
    layout: &Layout,
    environments: &mut Environments,
    events: &Sender<Event>,
    limit: usize,
) -> std::result::Result<(), String> {
    if layout.imports.is_empty() {
        return Ok(());
    }
    if service.is_some() {
        let root = declaration(events, json!({"operation": "orb"}), limit).await?;
        let root = root
            .as_u64()
            .filter(|root| environments.roots.contains_key(root))
            .ok_or("service environment has no declared Orb root")?;
        environments.parents.insert(environment, root);
        return Ok(());
    }
    let mut handles = BTreeMap::new();
    for import in &layout.imports {
        if import.contribution {
            continue;
        }
        let id = declaration(
            events,
            json!({
                "operation": "reference.declare",
                "service": import.binding,
                "key": [DEFAULT_KEY],
            }),
            limit,
        )
        .await?;
        let id = id
            .as_u64()
            .and_then(|id| u32::try_from(id).ok())
            .ok_or("invalid dependency reference handle")?;
        handles.insert(import.binding.clone(), id);
    }
    environments.roots.insert(environment, Arc::new(handles));
    Ok(())
}

/// Run one export as a staged concurrent call and settle it only after its task group finishes.
async fn invoke(
    engine: &Engine,
    store: &mut Store<State>,
    function: Func,
    arguments: &Value,
    authority: Authority,
    groups: &Arc<Mutex<Groups>>,
) -> wasmtime::Result<std::result::Result<String, String>> {
    let ty = function.ty(&*store);
    let Some(args) = arguments.as_array() else {
        return Ok(Err("component arguments require a tuple".into()));
    };
    if args.len() != ty.params().len() || ty.results().len() > 1 {
        return Ok(Err("component function arity differs".into()));
    }
    let args = match ty
        .params()
        .zip(args)
        .map(|((_, ty), value)| values::lower(ty, value))
        .collect::<tembra::Result<Vec<_>>>()
    {
        Ok(args) => args,
        Err(error) => return Ok(Err(error.to_string())),
    };
    let count = ty.results().len();
    store.set_fuel(engine.limits.fuel)?;
    let (finished, settled) = futures::channel::oneshot::channel();
    let call_work = crate::http::Ledger::default();
    // Captured at admission, while this call's epoch is the active one.
    let cancelled = futures::FutureExt::shared(Box::pin(lock(groups)?.scope.cancelled())
        as std::pin::Pin<Box<dyn Future<Output = ()> + Send>>);
    let commands = store.data().transport.commands.clone();
    let closing = store.data().adapter.closing.clone();
    // Bodies first read during this call are owned by it.
    commands.begin(Some(call_work.clone()));
    let watch = cancelled.clone();
    let groups = groups.clone();
    let call = store.run_concurrent(async move |accessor: &Accessor<State>| {
        let mut results = vec![Val::Bool(false); count];
        lock(&groups)?.pending = Some(Bound {
            authority,
            finished: Some(finished),
            call_work: call_work.clone(),
            cancelled,
        });
        let staged = accessor
            .with(|mut access| function.start_call_concurrent(&mut access, &args, &mut results));
        let staged = match staged {
            Ok(staged) => staged,
            Err(error) => {
                lock(&groups)?.pending = None;
                return Err(error);
            }
        };
        let group = staged.group();
        {
            let groups = lock(&groups)?;
            if groups.pending.is_some() || !groups.bound.contains_key(&group) {
                return Err(wasmtime::Error::msg(
                    "component call authority was not bound to its task group",
                ));
            }
        }
        let work = async {
            function.finish_call_concurrent(accessor, staged).await?;
            // A published result is not physical completion: wait until every task and
            // subtask of this call has exited, so no continuation can outlive its authority.
            settled
                .await
                .map_err(|_| wasmtime::Error::msg("component task group ended without finishing"))
        };
        {
            let mut work = std::pin::pin!(work);
            // The caller's cancellation closes the bodies this call is reading at once, so a
            // guest parked in a body read gets its end and returns; the call finishes normally.
            match futures::future::select(work.as_mut(), watch).await {
                futures::future::Either::Left((done, _)) => done?,
                futures::future::Either::Right(((), _)) => {
                    commands.cancel();
                    work.await?;
                }
            }
        }
        // Nor is a finished group: bodies it read close now, and a cancelled send may still own
        // a resolver lookup or a reactor connect. Release the call after they physically ended.
        commands.end();
        call_work.settled().await;
        // Socket streams the guest dropped during this call close physically before it ends,
        // and accepted sockets nobody will read are closed through upstream.
        closing.settled().await;
        accessor.with(|mut store| crate::worker::stream_adapter::release(store.data_mut()))?;
        crate::worker::stream_adapter::sweep(accessor).await?;
        Ok(results)
    });
    let results = tokio::time::timeout(
        std::time::Duration::from_millis(engine.limits.timeout_ms),
        call,
    )
    .await
    .map_err(|_| wasmtime::Error::msg("component call timed out"))???;
    let value = match results.into_iter().next().map(values::lift).transpose() {
        Ok(value) => value.unwrap_or(Value::Null),
        Err(error) => return Ok(Err(error.to_string())),
    };
    let text = value.to_string();
    if text.len() > engine.limits.packet_bytes {
        return Ok(Err("component result exceeds the packet limit".into()));
    }
    Ok(Ok(text))
}

/// Serve one incoming HTTP request as a staged call of the standard handler.
///
/// The same rules as [`invoke`] apply: one `run_concurrent` under the call timeout, authority
/// bound to the call's task group, the caller's cancellation ending its bodies, and completion
/// only after physical settlement. The difference is the result: the response head is
/// published on the call's own channel as soon as the guest returned it, while this Store keeps
/// driving the guest's body producer. The call completes only when its task group has finished
/// and the response body was delivered to its end, dropped by the application, or aborted.
async fn ingress(
    engine: &Engine,
    store: &mut Store<State>,
    handler: Handler,
    call: crate::ingress::IngressCall,
    authority: Authority,
    groups: &Arc<Mutex<Groups>>,
) -> wasmtime::Result<std::result::Result<String, String>> {
    let crate::ingress::IngressCall {
        request,
        exchange,
        head,
    } = call;
    store.set_fuel(engine.limits.fuel)?;
    let (finished, settled) = futures::channel::oneshot::channel();
    let call_work = crate::http::Ledger::default();
    let cancelled = futures::FutureExt::shared(Box::pin(lock(groups)?.scope.cancelled())
        as std::pin::Pin<Box<dyn Future<Output = ()> + Send>>);
    let commands = store.data().transport.commands.clone();
    let closing = store.data().adapter.closing.clone();
    commands.begin(Some(call_work.clone()));
    let watch = cancelled.clone();
    let groups = groups.clone();
    let failed = exchange.clone();
    let call = store.run_concurrent(async move |accessor: &Accessor<State>| {
        // The request becomes a resource of this Store; its body is read by upstream's host
        // producer, which this call can close through the exchange.
        let (request, received) = accessor.with(|mut access| {
            let state = access.get();
            let (request, received) = p3::Request::from_http(&mut state.hooks, request);
            state.table.push(request).map(|request| (request, received))
        })?;
        lock(&groups)?.pending = Some(Bound {
            authority,
            finished: Some(finished),
            call_work: call_work.clone(),
            cancelled,
        });
        let staged =
            accessor.with(|mut access| handler.start_call_concurrent(&mut access, (request,)));
        let staged = match staged {
            Ok(staged) => staged,
            Err(error) => {
                lock(&groups)?.pending = None;
                return Err(error);
            }
        };
        let group = staged.group();
        {
            let groups = lock(&groups)?;
            if groups.pending.is_some() || !groups.bound.contains_key(&group) {
                return Err(wasmtime::Error::msg(
                    "component call authority was not bound to its task group",
                ));
            }
        }
        let respond = exchange.clone();
        let work = async {
            let (result,) = handler.finish_call_concurrent(accessor, staged).await?;
            // The handler's own error refuses this request only; the Store stays usable.
            let mut refused = None;
            match result {
                Ok(response) => {
                    let response = accessor.with(|mut access| {
                        let response = access.get().table.delete(response)?;
                        response.into_http(&mut access, respond.transmission(received))
                    })?;
                    // The head leaves now; the call stays admitted and keeps its pin.
                    crate::ingress::IngressCall::publish(&respond, head, response);
                }
                Err(code) => {
                    respond.absent();
                    let message = format!("the incoming HTTP handler returned {code:?}");
                    let _ = head.send(Err(tembra::Error::new(
                        tembra::core::Code::Unavailable,
                        "wasi_http_handler",
                        message.clone(),
                    )));
                    refused = Some(message);
                }
            }
            // Physical completion: every task of the call exited and the body's delivery ended.
            let delivered = respond.delivered();
            let finished = async {
                settled.await.map_err(|_| {
                    wasmtime::Error::msg("component task group ended without finishing")
                })
            };
            let (finished, _) = futures::future::join(finished, delivered).await;
            finished.map(|()| refused)
        };
        let refused = {
            let mut work = std::pin::pin!(work);
            // Cancellation closes this call's bodies, including both ingress bodies, so a guest
            // parked in a read or write gets its end and returns; the call finishes normally.
            match futures::future::select(work.as_mut(), watch).await {
                futures::future::Either::Left((done, _)) => done?,
                futures::future::Either::Right(((), _)) => {
                    commands.cancel();
                    exchange.abort();
                    work.await?
                }
            }
        };
        commands.end();
        call_work.settled().await;
        closing.settled().await;
        accessor.with(|mut store| crate::worker::stream_adapter::release(store.data_mut()))?;
        crate::worker::stream_adapter::sweep(accessor).await?;
        Ok(refused)
    });
    let outcome = tokio::time::timeout(
        std::time::Duration::from_millis(engine.limits.timeout_ms),
        call,
    )
    .await
    .map_err(|_| wasmtime::Error::msg("component call timed out"))
    .and_then(|result| result.and_then(|result| result));
    // A terminal failure ends the response with an error before the Store, and the guest's
    // stream with it, is dropped: a truncated body never reads as complete.
    if outcome.is_err() {
        failed.fail();
    }
    // A handler error completes the command with that error (after settlement); platform hosts
    // read it from the completion, the native facade from its head channel.
    Ok(match outcome? {
        Some(refused) => Err(refused),
        None => Ok("null".into()),
    })
}

fn lock(groups: &Mutex<Groups>) -> wasmtime::Result<std::sync::MutexGuard<'_, Groups>> {
    groups
        .lock()
        .map_err(|_| wasmtime::Error::msg("task-group attribution is poisoned"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use wasmtime_wasi_http::{WasiBody, WasiHttpHooks};

    fn request(url: &str) -> http::Request<WasiBody> {
        http::Request::get(url)
            .body(
                http_body_util::Empty::new()
                    .map_err(|never| match never {})
                    .boxed_unsync(),
            )
            .unwrap()
    }

    fn hooks() -> crate::http::Hooks {
        crate::http::Hooks::new(
            Arc::new(crate::Grants {
                http: vec![tembra_contracts::HttpGrant {
                    provider: "web".into(),
                    origins: vec!["http://127.0.0.1:9".into()],
                }],
                ..Default::default()
            }),
            Arc::new(GroupAuthority(Arc::default())),
            crate::http::Transport::default(),
        )
    }

    /// Requests from initializers or tasks outside an admitted call's group are denied.
    #[test]
    fn requests_without_call_authority_are_denied() {
        let mut hooks = hooks();
        let sent = hooks.send_request(
            request("http://127.0.0.1:9/"),
            None,
            Box::new(async { Ok(()) }),
        );
        let error = futures::executor::block_on(Box::into_pin(sent))
            .err()
            .unwrap();
        assert!(matches!(
            error,
            wasmtime_wasi_http::Error::HttpRequestDenied
        ));
    }

    /// The pre-send check refuses when the originating call ended or its adapter refuses.
    #[test]
    fn context_check_follows_the_originating_call() {
        let (events, receive) = async_channel::bounded(1);
        drop(receive);
        assert!(futures::executor::block_on(check_context(&events)).is_err());
        for (answer, admitted) in [
            (Ok("null".to_owned()), true),
            (Err("cancelled".into()), false),
        ] {
            let (events, receive) = async_channel::bounded(1);
            let responder = std::thread::spawn(move || {
                let Ok(Event::Host {
                    packet,
                    asynchronous,
                    reply,
                }) = receive.recv_blocking()
                else {
                    panic!("expected a host request");
                };
                assert_eq!(packet, r#"{"operation":"context.check"}"#);
                assert!(!asynchronous);
                let _ = reply.send(answer);
            });
            assert_eq!(
                futures::executor::block_on(check_context(&events)).is_ok(),
                admitted
            );
            responder.join().unwrap();
        }
    }
}
