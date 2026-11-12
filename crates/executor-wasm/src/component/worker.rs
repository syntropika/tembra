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
//! Kernel-managed state follows the same path. A component exporting `tembra:guest/state-init`
//! is asked, while each service object is created, for that object's declaration; the worker
//! declares it through the existing synchronous `state.declare` request, with a worker-numbered
//! validation callback when the declaration asks for one, which the kernel runs through a
//! `validate` command before a reload adopts the retained value. The `tembra:guest/state`
//! imports then issue `state.read` and `state.update` on the channel of the call that physically
//! issued them, for the handle of the object that call runs on.
//!
//! Root resources follow the TypeScript guest model. A component exporting
//! `tembra:guest/resources-init` is asked for its declarations while the root is created; each is
//! declared through the existing `resource.declare` request with worker-issued callback numbers,
//! and the kernel later runs the component's `open` and `close` through `callback` commands. The
//! kernel keeps only the handle the component returned; the value stays in the component.
//!
//! A component declaring hooks exports each hook's interface, as a source provider does, and
//! imports it as the typed entry to its plan. While the root is created the worker binds each
//! declared hook through the existing `hook.declare` request; a call of the import issues
//! `hook.run` on the channel of the call that physically issued it, and the kernel runs the
//! plan of contributions exactly as it does for a source provider.
//!
//! A component importing `tembra:guest/context` reads the kernel's view of its call: the
//! principal and the cancellation of the call that physically issued the import, through the
//! existing `context.principal`, `signal.cancelled` and `context.cancel` requests, the host's
//! log through `console`, and the address of the object the call runs on, which the worker asks
//! for with `address` while each environment is declared and keeps for its calls.
//!
//! Standard `wasi:http` imports use upstream `wasmtime-wasi-http` P3 resources and its sender.
//! Each request is attributed the same way as a dependency import: the hook captures the entered
//! task group's authority synchronously, checks the installation's HTTP origin grants and then
//! asks the originating call, through its own channel, whether its context is still admitted.
use super::values;
mod arguments;
use crate::{
    Engine,
    abi::Operation,
    worker::{Command, Event, Pipe, PipeConsumer, StreamCall, Terminal},
};
use async_channel::Sender;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{Arc, Mutex, mpsc},
};
use tembra_contracts::guest;
use tokio::sync::oneshot;
use wasmtime::{
    Store, StoreLimits, StoreLimitsBuilder,
    component::{
        Accessor, Component, Func, Linker, Resource, ResourceTable, ResourceType, TaskGroupHook,
        TaskGroupId, TypedFunc, Val, types::ComponentFunc,
    },
};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_wasi_http::p3::{self, bindings::http::types::ErrorCode};
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpView};

/// Default dependency object selected by a plain WIT import, which carries no object key.
const DEFAULT_KEY: &str = "main";

/// Exported callbacks and selected Orb imports, resolved from the prepared contract.
pub(crate) struct Layout {
    /// Callable export interfaces: service methods and contribution callbacks.
    pub exports: Vec<Export>,
    /// Orb-bound import interfaces, in world order.
    pub imports: Vec<Import>,
    /// Function ids of the checked contribution callbacks; never ordinary service methods.
    pub contributions: BTreeSet<u32>,
    /// The exact incoming HTTP handler export, served only through the ingress adapter.
    pub ingress: Option<String>,
    /// The component exports `tembra:guest/state-init`: its service objects may own kernel state.
    pub state_init: bool,
    /// The component exports `tembra:guest/resources-init`: its root owns resources.
    pub resource_init: bool,
    /// The hooks this Orb declares: the import binding of each, to the alias of its export,
    /// which `hook.declare` names.
    pub hooks: BTreeMap<String, String>,
    /// The component imports `tembra:guest/context`: its calls read the kernel's view of them.
    pub context: bool,
}

/// The standard handler's `handle`, typed against the host's own request and response
/// resources. Typing it at instantiation checks the actual binary's signature.
type Handler = TypedFunc<(Resource<p3::Request>,), (Result<Resource<p3::Response>, ErrorCode>,)>;

/// Callable exports resolved once per Store.
struct Exports {
    /// Each callable export by its function id, the export slot `bind` returned.
    functions: BTreeMap<u32, Target>,
    /// The kernel's own callbacks (`tembra:guest` state and resource initialisation), by
    /// interface and function. They have no slot and belong to no service, so a host command
    /// can never select them; only the worker runs them, for the kernel.
    kernel: BTreeMap<(String, String), Func>,
    handler: Option<Handler>,
}

/// One resolved export function and the only service whose environments may call it.
#[derive(Clone)]
struct Target {
    function: Func,
    service: Arc<str>,
    /// The actual resource types if it lends `self` or returns an owned resource.
    roles: Resolved,
}

/// Resource positions of one export, as actual types of this Store.
#[derive(Clone, Default)]
struct Resolved {
    receiver: Option<ResourceType>,
    owned_results: bool,
}

pub(crate) struct Export {
    pub interface: String,
    /// JavaScript service alias whose environments may call these functions.
    pub service: String,
    /// Function id (export slot) to original WIT function name and resource positions.
    pub functions: BTreeMap<u32, ExportFunction>,
}

pub(crate) struct ExportFunction {
    pub name: String,
    /// Resource positions; absent for data-only functions.
    pub roles: Option<Roles>,
}

/// Resource positions of one function, by the local name of the defining resource.
pub(crate) struct Roles {
    pub receiver: Option<String>,
    pub owned_results: bool,
    pub owned_arguments: bool,
}

pub(crate) struct Import {
    /// World import key; the embedding adapter maps it to the selected provider service.
    pub binding: String,
    /// Imported resource types and the dynamic host type each is linked as.
    pub resources: Vec<(String, u32)>,
    pub functions: Vec<ImportFunction>,
    pub role: Role,
    /// This interface acquires root-owned references explicitly, without a default main.
    pub reference: bool,
}

/// What an import of the checked layout is linked to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    /// A selected Orb provider, called through the root's dependency reference.
    Dependency,
    /// A type anchor of a contribution; calling it traps.
    Contribution,
    /// One of the kernel's own interfaces (state, resources, context), served for the call that
    /// issues it.
    Kernel,
    /// The typed entry to a hook this Orb declares; calling it runs the plan through the kernel.
    Hook,
}

pub(crate) struct ImportFunction {
    pub name: String,
    /// JavaScript spelling used by the private `reference.call` request.
    pub spelling: String,
    /// `async func` imports are linked as concurrent host functions.
    pub asynchronous: bool,
    /// The dynamic type of the implicit `self` borrow, for resource methods.
    pub receiver: Option<u32>,
    /// Whether the result contains owned resources, including nested positions.
    pub owned_results: bool,
    /// Dynamic resource types linked by this import, for recursive result lowering.
    pub payloads: Vec<u32>,
    /// The whole result is a `stream<u8>`, read from a pipe the host fills.
    pub stream: bool,
    /// Dynamic host resource type returned inside the acquisition result.
    pub acquire: Option<u32>,
}

/// Immutable authority of one admitted call, shared by every thread of its task group.
#[derive(Clone)]
struct Authority {
    events: Sender<Event>,
    /// Orb root environment whose handle table holds the dependency references and every
    /// owned imported resource of this Store.
    scope: u64,
    handles: Arc<BTreeMap<String, u32>>,
    /// The environment the call runs in: the root for contributions, else a service object.
    environment: u64,
    /// The kernel state handle of that service object, once declared.
    state: Option<u32>,
    /// The root's resource handles by name, for the kernel's resource reads.
    resources: Arc<BTreeMap<String, u32>>,
    /// The root's hook handles by import binding, for running the hooks this Orb declares.
    hooks: Arc<BTreeMap<String, u32>>,
    /// The kernel address of the environment's object (key and principal), for the context
    /// interface; declared only for components importing it.
    address: Option<Arc<Value>>,
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
    /// Exported resources held for native owners, by issuing environment.
    instances: super::resources::Instances,
    /// Imported resource slots the guest dropped, released at the end of the current call.
    dropped: Dropped,
    /// Only these host-dynamic types were linked from checked Orb imports.
    payloads: BTreeSet<u32>,
    /// The session's diagnostics: the context interface's log reaches the host's log observers
    /// through them, as a source Orb's console records do, never as a host request.
    diagnostics: crate::worker::diagnostics::Diagnostics,
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
/// resolver instead of upstream's untracked lookup jobs, and clock/socket waits released by the
/// caller's cancellation.
fn link_wasi(linker: &mut Linker<State>) -> wasmtime::Result<()> {
    use wasmtime_wasi::clocks::{WasiClocks, WasiClocksView};
    use wasmtime_wasi::p3;
    p3::bindings::clocks::types::add_to_linker::<State, WasiClocks>(linker, State::clocks)?;
    p3::bindings::clocks::system_clock::add_to_linker::<State, WasiClocks>(linker, State::clocks)?;
    p3::bindings::clocks::monotonic_clock::add_to_linker::<State, crate::clocks::CallClocks>(
        linker,
        State::clocks,
    )?;
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
        function: u32,
        args: Value,
        /// The host-issued id of the exported resource lent as implicit `self`.
        #[serde(default)]
        receiver: Option<u32>,
    },
    Contribute {
        environment: u64,
        function: u32,
        args: Value,
    },
    Release {
        environment: u64,
    },
    Destroy {
        environment: u64,
        instance: u32,
    },
    Ingress {
        environment: u64,
    },
    /// A resource opener or closer the worker numbered in its `resource.declare` request.
    Callback {
        callback: u32,
        #[serde(default)]
        value: Option<u32>,
    },
    /// The validator the worker numbered in a `state.declare` request, with the retained value
    /// a reload would adopt.
    Validate {
        callback: u32,
        value: Value,
    },
    #[serde(other)]
    Unsupported,
}

/// Dependency handles issued to Orb root environments, each service's root, the kernel state
/// handle declared by each service object, and the root's resources with their callbacks.
#[derive(Default)]
struct Environments {
    roots: BTreeMap<u64, Arc<BTreeMap<String, u32>>>,
    parents: BTreeMap<u64, u64>,
    /// Every declared, unreleased environment and its service alias (`None` for an Orb root).
    services: BTreeMap<u64, Option<String>>,
    states: BTreeMap<u64, u32>,
    resources: BTreeMap<u64, Arc<BTreeMap<String, u32>>>,
    callbacks: BTreeMap<u32, Callback>,
    next_callback: u32,
    hooks: BTreeMap<u64, Arc<BTreeMap<String, u32>>>,
    /// The kernel address of each environment's object, asked while it was declared.
    addresses: BTreeMap<u64, Arc<Value>>,
}

/// One callback the worker numbered in a declaration, for the kernel to run later.
#[derive(Clone)]
enum Callback {
    /// The opener or the closer of a root resource.
    Resource {
        environment: u64,
        name: String,
        open: bool,
    },
    /// The validator of a service object's state: the interface identity `validate` is told.
    State { environment: u64, interface: String },
}

impl Callback {
    fn environment(&self) -> u64 {
        match self {
            Self::Resource { environment, .. } | Self::State { environment, .. } => *environment,
        }
    }
}

impl Environments {
    /// Number one more callback.
    fn callback(&mut self) -> std::result::Result<u32, String> {
        let next = self
            .next_callback
            .checked_add(1)
            .ok_or("callback space exhausted")?;
        self.next_callback = next;
        Ok(next)
    }
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
    // The session's callback epochs, quarantine cause and diagnostics travel with its command
    // queue.
    (commands, scope, poison, diagnostics): (
        mpsc::Receiver<Command>,
        crate::io::CallbackScope,
        Arc<Mutex<Option<String>>>,
        crate::worker::diagnostics::Diagnostics,
    ),
) -> Terminal {
    let transport = crate::http::Transport::default();
    let terminal = live(
        (engine, component, wasi, grants, layout),
        ready,
        (commands, scope, poison),
        (&transport, diagnostics),
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
    (commands, scope, poison): (
        mpsc::Receiver<Command>,
        crate::io::CallbackScope,
        Arc<Mutex<Option<String>>>,
    ),
    (transport, diagnostics): (
        &crate::http::Transport,
        crate::worker::diagnostics::Diagnostics,
    ),
) -> Terminal {
    let groups = Arc::new(Mutex::new(Groups {
        scope,
        ..Groups::default()
    }));
    let instance = instantiate(
        &engine,
        &component,
        wasi,
        grants,
        &layout,
        &groups,
        (transport, diagnostics),
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
                // Record the cause before the queue closes: a command nobody awaits (a resource
                // destruction or a release) has no caller to report it, and a call already
                // queued behind it must see why the revision stopped.
                if let Ok(mut poison) = poison.lock() {
                    poison.get_or_insert_with(|| message.clone());
                }
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
    (transport, diagnostics): (
        &crate::http::Transport,
        crate::worker::diagnostics::Diagnostics,
    ),
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
            instances: super::resources::Instances::new(engine.live_resources.clone()),
            dropped: Dropped::default(),
            payloads: layout
                .imports
                .iter()
                .flat_map(|import| import.resources.iter().map(|(_, payload)| *payload))
                .collect(),
            diagnostics,
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
    for export in &layout.exports {
        let service = Arc::<str>::from(export.service.as_str());
        let (_, interface) = instance
            .get_export(&mut store, None, &export.interface)
            .ok_or_else(|| wasmtime::Error::msg("missing component interface"))?;
        // The binary's own resource types: only handles of exactly these are ever kept or lent.
        let mut resource = |name: &Option<String>| {
            name.as_ref()
                .map(|name| {
                    instance
                        .get_export(&mut store, Some(&interface), name)
                        .and_then(|(_, index)| instance.get_resource(&mut store, index))
                        .ok_or_else(|| wasmtime::Error::msg("missing component resource type"))
                })
                .transpose()
        };
        let mut roles = BTreeMap::new();
        for (id, exported) in &export.functions {
            if let Some(role) = &exported.roles {
                let resolved = Resolved {
                    receiver: resource(&role.receiver)?,
                    owned_results: role.owned_results,
                };
                roles.insert(*id, resolved);
            }
        }
        for (id, exported) in &export.functions {
            let roles = roles.remove(id).unwrap_or_default();
            let (_, index) = instance
                .get_export(&mut store, Some(&interface), &exported.name)
                .ok_or_else(|| wasmtime::Error::msg("missing component method"))?;
            let function = instance
                .get_func(&mut store, index)
                .ok_or_else(|| wasmtime::Error::msg("component export is not a function"))?;
            let target = Target {
                function,
                service: service.clone(),
                roles,
            };
            if exports.insert(*id, target).is_some() {
                return Err(wasmtime::Error::msg("duplicate component function id"));
            }
        }
    }
    let mut kernel = BTreeMap::new();
    let mut kernel_callbacks = |interface: &str, functions: &[&str]| -> wasmtime::Result<()> {
        let (_, export) = instance
            .get_export(&mut store, None, interface)
            .ok_or_else(|| wasmtime::Error::msg(format!("missing kernel export {interface}")))?;
        for function in functions {
            let (_, index) = instance
                .get_export(&mut store, Some(&export), function)
                .ok_or_else(|| {
                    wasmtime::Error::msg(format!("missing kernel callback {interface}#{function}"))
                })?;
            let callback = instance.get_func(&mut store, index).ok_or_else(|| {
                wasmtime::Error::msg(format!(
                    "kernel export {interface}#{function} is not a function"
                ))
            })?;
            kernel.insert((interface.to_owned(), (*function).to_owned()), callback);
        }
        Ok(())
    };
    if layout.state_init {
        kernel_callbacks(guest::STATE_INIT, &[guest::DECLARE, guest::VALIDATE])?;
    }
    if layout.resource_init {
        kernel_callbacks(
            guest::RESOURCE_INIT,
            &[guest::DECLARE, guest::OPEN, guest::CLOSE],
        )?;
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
            kernel,
            handler,
        },
    ))
}

/// Define each selected dependency function and the kernel's state functions; WASI imports
/// remain the installation's grants.
///
/// An imported resource is a dynamic host type per import and resource. Its representation is
/// a slot of the importer's own host handle table; dropping it queues the slot's release.
fn link_imports(linker: &mut Linker<State>, layout: &Layout) -> wasmtime::Result<()> {
    for import in &layout.imports {
        let mut instance = linker.instance(&import.binding)?;
        for (name, payload) in &import.resources {
            instance.resource(name, ResourceType::host_dynamic(*payload), |store, slot| {
                if let Ok(mut dropped) = store.data().dropped.lock() {
                    dropped.push(slot);
                }
                Ok(())
            })?;
        }
        for function in &import.functions {
            match import.role {
                Role::Contribution => {
                    let message = format!(
                        "hook import {} is a contribution type anchor and cannot be called",
                        import.binding
                    );
                    instance.func_new(&function.name, move |_, _, _, _| {
                        Err(wasmtime::Error::msg(message.clone()))
                    })?;
                    continue;
                }
                Role::Kernel => {
                    let name = function.name.clone();
                    match import.binding.as_str() {
                        guest::STATE => {
                            instance.func_new_async(
                                &function.name,
                                move |store, _, params, results| {
                                    let request = state_request(store.data(), &name, params);
                                    Box::new(async move { settle_state(request?, results).await })
                                },
                            )?;
                        }
                        guest::RESOURCE => {
                            instance.func_new_async(
                                &function.name,
                                move |store, _, params, results| {
                                    let request = resource_request(store.data(), &name, params);
                                    Box::new(
                                        async move { settle_resource(request?, results).await },
                                    )
                                },
                            )?;
                        }
                        guest::CONTEXT => {
                            instance.func_new_async(
                                &function.name,
                                move |store, ty, params, results| {
                                    let answer = context_request(store.data(), &name, params);
                                    Box::new(
                                        async move { settle_context(answer?, &ty, results).await },
                                    )
                                },
                            )?;
                        }
                        other => {
                            return Err(wasmtime::Error::msg(format!(
                                "unsupported kernel import {other}"
                            )));
                        }
                    }
                    continue;
                }
                Role::Hook => {
                    let binding = import.binding.clone();
                    // An `async func` callback is linked as a concurrent host function, as a
                    // dependency's is; Wasmtime refuses a synchronous one for it.
                    if function.asynchronous {
                        instance.func_new_concurrent(
                            &function.name,
                            move |accessor: &Accessor<State>, ty, params, results| {
                                // The first poll runs inside the issuing guest call, so the
                                // entered group is still the caller's when its authority is
                                // captured.
                                let request = accessor.with(|mut access| {
                                    hook_request(access.get(), &binding, params)
                                });
                                Box::pin(async move {
                                    let value = hook_settle(request?).await?;
                                    store_result(results, lower_result(&ty, &value)?)
                                })
                            },
                        )?;
                    } else {
                        instance.func_new_async(
                            &function.name,
                            move |store, ty, params, results| {
                                let request = hook_request(store.data(), &binding, params);
                                Box::new(async move {
                                    let value = hook_settle(request?).await?;
                                    store_result(results, lower_result(&ty, &value)?)
                                })
                            },
                        )?;
                    }
                    continue;
                }
                Role::Dependency => {}
            }
            let target = Arc::new((import.binding.clone(), function.spelling.clone()));
            if let Some(payload) = function.acquire {
                instance.func_new_concurrent(
                    &function.name,
                    move |accessor: &Accessor<State>, _, params, results| {
                        // Capture this task group's authority before the import can suspend.
                        let request = accessor.with(|mut access| {
                            acquire_request(access.get(), &target.0, params).map(|receive| {
                                ImportReply::new(receive, Some(access.get().dropped.clone()))
                            })
                        });
                        Box::pin(async move {
                            let mut reply = request?;
                            // A refused key is request data: return the WIT error rather than
                            // trapping and quarantining every object in this revision.
                            let result = match reply.answer().await? {
                                Ok(value) => Ok(accessor.with(|mut access| {
                                    owned(&mut access, &value, payload)
                                        .map(|value| value.map(Box::new))
                                })?),
                                Err(message) => Err(Some(Box::new(Val::String(message)))),
                            };
                            store_result(results, Some(Val::Result(result)))?;
                            reply.delivered();
                            Ok(())
                        })
                    },
                )?;
                continue;
            }
            let receiver = function.receiver;
            let owned_results = function.owned_results;
            let payloads = Arc::new(function.payloads.clone());
            if function.stream {
                instance.func_new_concurrent(
                    &function.name,
                    move |accessor: &Accessor<State>, _, params, results| {
                        // The pipe is this Store's own; the host only fills it. It crosses as a
                        // typed value in the request event, never as a number.
                        let request = accessor.with(|mut access| {
                            let pipe = Arc::<Mutex<Pipe>>::default();
                            request(&mut access, &target, None, params, Some(pipe.clone())).map(
                                |receive| StreamReply {
                                    reply: ImportReply::new(receive, None),
                                    pipe,
                                    held: false,
                                },
                            )
                        });
                        Box::pin(async move {
                            let mut reply = request?;
                            if reply.reply.settle().await? != json!(0) {
                                return Err(wasmtime::Error::msg(
                                    "a stream import result names no stream",
                                ));
                            }
                            let stream = accessor.with(|mut access| {
                                let reader = crate::worker::stream_adapter::guest_reader(
                                    wasmtime::AsContextMut::as_context_mut(&mut access),
                                    reply.pipe.clone(),
                                )?;
                                wasmtime::component::StreamAny::try_from_stream_reader(
                                    &mut access,
                                    reader,
                                )
                            })?;
                            reply.held = true;
                            reply.reply.delivered();
                            store_result(results, Some(Val::Stream(stream)))
                        })
                    },
                )?;
                continue;
            }
            if function.asynchronous {
                instance.func_new_concurrent(
                    &function.name,
                    move |accessor: &Accessor<State>, ty, params, results| {
                        // The first poll runs inside the issuing guest call, so the entered group
                        // is still the caller's when its authority is captured.
                        let request = accessor.with(|mut access| {
                            let (selected, params) = select(&mut access, receiver, params)?;
                            let dropped = owned_results.then(|| access.get().dropped.clone());
                            request(&mut access, &target, selected, params, None).map(|receive| {
                                ImportReply::new(receive, dropped).with_result(ty.results().next())
                            })
                        });
                        let payloads = payloads.clone();
                        Box::pin(async move {
                            // The guest may abandon this call at any await; the reply guard then
                            // queues a committed owned slot for release instead of stranding it.
                            let mut reply = request?;
                            let value = reply.settle().await?;
                            let value = accessor.with(|mut access| {
                                lower_owned_result(&mut access, &ty, &value, &payloads)
                            })?;
                            reply.delivered();
                            store_result(results, value)
                        })
                    },
                )?;
            } else {
                instance.func_new_async(
                    &function.name,
                    move |mut store, ty, params, results| {
                        let dropped = owned_results.then(|| store.data().dropped.clone());
                        let request =
                            select(&mut store, receiver, params).and_then(|(selected, params)| {
                                request(&mut store, &target, selected, params, None).map(
                                    |receive| {
                                        ImportReply::new(receive, dropped)
                                            .with_result(ty.results().next())
                                    },
                                )
                            });
                        let payloads = payloads.clone();
                        Box::new(async move {
                            let mut reply = request?;
                            let value = reply.settle().await?;
                            let value = lower_owned_result(&mut store, &ty, &value, &payloads)?;
                            reply.delivered();
                            store_result(results, value)
                        })
                    },
                )?;
            }
        }
    }
    Ok(())
}

/// Split a resource method's implicit `self` from its data arguments. The borrow names a slot
/// of this importer's own table; nothing else can select an owned resource.
fn select(
    store: impl wasmtime::AsContextMut,
    receiver: Option<u32>,
    params: &[Val],
) -> wasmtime::Result<(Option<u32>, &[Val])> {
    match (receiver, params.split_first()) {
        (Some(payload), Some((lent, rest))) => Ok((
            Some(super::resources::lent_slot(store, lent, payload)?),
            rest,
        )),
        (Some(_), None) => Err(wasmtime::Error::msg("resource method has no self")),
        (None, _) => Ok((None, params)),
    }
}

/// Give the guest its owned handle to the slot the host installed for this result.
fn owned(
    store: impl wasmtime::AsContextMut,
    value: &Value,
    payload: u32,
) -> wasmtime::Result<Option<Val>> {
    let slot = value
        .as_u64()
        .and_then(|slot| u32::try_from(slot).ok())
        .ok_or_else(|| wasmtime::Error::msg("owned import result names no handle slot"))?;
    Ok(Some(super::resources::owned_value(store, slot, payload)?))
}

/// Lower a checked data result with the import's actual type.
fn lower_result(ty: &ComponentFunc, value: &Value) -> wasmtime::Result<Option<Val>> {
    match ty.results().next() {
        Some(ty) => {
            Ok(Some(values::lower(ty, value).map_err(|error| {
                wasmtime::Error::msg(error.to_string())
            })?))
        }
        None if value.is_null() => Ok(None),
        None => Err(wasmtime::Error::msg(
            "dependency result differs from its import",
        )),
    }
}

/// Lower nested resource slots only through dynamic types linked by this exact import.
fn lower_owned_result(
    mut store: impl wasmtime::AsContextMut,
    ty: &ComponentFunc,
    value: &Value,
    payloads: &[u32],
) -> wasmtime::Result<Option<Val>> {
    let Some(ty) = ty.results().next() else {
        return lower_result(ty, value);
    };
    values::lower_with(ty, value, &mut |expected, value| {
        let payload = payloads
            .iter()
            .copied()
            .find(|payload| ResourceType::host_dynamic(*payload) == expected)
            .ok_or_else(|| crate::failure("result resource is not linked by this import"))?;
        let slot = slot(value).ok_or_else(|| crate::failure("resource result names no slot"))?;
        super::resources::owned_value(&mut store, slot, payload).map_err(crate::failure)
    })
    .map(Some)
    .map_err(|error| wasmtime::Error::msg(error.to_string()))
}

/// Store the single result an import function returns, if it has one.
fn store_result(results: &mut [Val], value: Option<Val>) -> wasmtime::Result<()> {
    match (results.first_mut(), value) {
        (Some(result), Some(value)) => *result = value,
        (None, None) => {}
        _ => {
            return Err(wasmtime::Error::msg(
                "dependency result differs from its import",
            ));
        }
    }
    Ok(())
}

/// Capture the authority of the guest thread issuing an import, before any suspension.
fn import_authority(state: &State) -> wasmtime::Result<Authority> {
    issuing_authority(state, "dependency import has no admitted invocation")
}

/// Acquire a kernel object using only the checked import binding and guest-chosen key.
fn acquire_request(
    state: &State,
    binding: &str,
    params: &[Val],
) -> wasmtime::Result<oneshot::Receiver<std::result::Result<String, String>>> {
    let authority = import_authority(state)?;
    let [key] = params else {
        return Err(wasmtime::Error::msg(
            "reference acquisition requires one key",
        ));
    };
    let key = values::lift(key.clone()).map_err(|error| wasmtime::Error::msg(error.to_string()))?;
    let packet =
        json!({"operation": "reference.acquire", "service": binding, "key": key}).to_string();
    let (reply, receive) = oneshot::channel();
    if packet.len() > state.packet_bytes {
        let _ = reply.send(Err("host packet limit exceeded".into()));
        return Ok(receive);
    }
    if let Err(error) = authority.events.try_send(Event::Host {
        packet,
        asynchronous: true,
        reply,
    }) {
        let Event::Host { reply, .. } = error.into_inner() else {
            unreachable!("acquisition sends a host request")
        };
        let _ = reply.send(Err("host operation queue is full or closed".into()));
    }
    Ok(receive)
}

/// Capture the issuing task group's authority and publish one bounded `reference.call` request.
///
/// `selected` is the importer table slot a resource method's `self` named; every other function
/// calls through the import's declared reference.
fn request(
    mut store: impl wasmtime::AsContextMut<Data = State>,
    (binding, method): &(String, String),
    selected: Option<u32>,
    params: &[Val],
    stream: Option<Arc<Mutex<Pipe>>>,
) -> wasmtime::Result<oneshot::Receiver<std::result::Result<String, String>>> {
    let authority = import_authority(store.as_context().data())?;
    let id = match selected {
        Some(slot) => slot,
        None => *authority
            .handles
            .get(binding)
            .ok_or_else(|| wasmtime::Error::msg("dependency import has no declared reference"))?,
    };
    let dropped = store.as_context().data().dropped.clone();
    let mut moves = crate::worker::arguments::Moves::new(Vec::new(), dropped);
    let args = params
        .iter()
        .cloned()
        .map(|value| {
            values::lift_with(value, &mut |resource| {
                if !resource.owned() {
                    return Err(crate::failure("only owned argument resources can transfer"));
                }
                let linked = store
                    .as_context()
                    .data()
                    .payloads
                    .iter()
                    .any(|payload| ResourceType::host_dynamic(*payload) == resource.ty());
                if !linked {
                    return Err(crate::failure(
                        "a fresh guest resource has no issued ownership claim",
                    ));
                }
                let resource = wasmtime::component::ResourceDynamic::try_from_resource_any(
                    resource, &mut store,
                )
                .map_err(crate::failure)?;
                Ok(json!(moves.push(resource.rep())))
            })
        })
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
    if packet.len() > store.as_context().data().packet_bytes {
        return Err(wasmtime::Error::msg("host packet limit exceeded"));
    }
    let (reply, receive) = oneshot::channel();
    let event = if !moves.is_empty() {
        Event::Arguments {
            packet,
            moves,
            reply,
            pipe: stream,
        }
    } else {
        match stream {
            Some(pipe) => Event::Stream {
                packet,
                reply,
                pipe,
            },
            None => Event::Host {
                packet,
                asynchronous: true,
                reply,
            },
        }
    };
    authority
        .events
        .try_send(event)
        .map_err(|_| wasmtime::Error::msg("host operation queue is full or closed"))?;
    Ok(receive)
}

/// The pending reply of one stream import, and the pipe its result will be read from.
///
/// Until the guest holds its reader, this guard owns the pipe: if the guest abandons the call
/// or the reader cannot be made, dropping the guard closes the pipe, so the host's pump drops
/// the delivered end and the producer is asked to stop exactly once. Afterwards the reader owns
/// the pipe and closes it when the guest drops it.
struct StreamReply {
    reply: ImportReply,
    pipe: Arc<Mutex<Pipe>>,
    held: bool,
}

impl Drop for StreamReply {
    fn drop(&mut self) {
        if !self.held {
            self.pipe.lock().expect("pipe").close();
        }
    }
}

/// Imported resource slots dropped by the guest, or abandoned before the guest held them,
/// waiting to be released on a host channel.
type Dropped = Arc<Mutex<Vec<u32>>>;

/// The pending reply of one guest import call.
///
/// The host commits an owned result's slot once this Store has received the reply. Until the
/// guest actually holds the handle, this guard owns the slot: if the guest abandons the call
/// (an async import future dropped mid-call) or the handle cannot be made, dropping the guard
/// queues the slot for release, exactly like a handle the guest dropped.
struct ImportReply {
    receive: oneshot::Receiver<std::result::Result<String, String>>,
    /// The release queue, for an import whose result is an owned resource.
    owned: Option<Dropped>,
    /// The slot received but not yet held by the guest.
    pending: Vec<u32>,
    result: Option<wasmtime::component::Type>,
    delivered: bool,
}

impl ImportReply {
    fn new(
        receive: oneshot::Receiver<std::result::Result<String, String>>,
        owned: Option<Dropped>,
    ) -> Self {
        Self {
            receive,
            owned,
            pending: Vec::new(),
            result: None,
            delivered: false,
        }
    }

    fn with_result(mut self, result: Option<wasmtime::component::Type>) -> Self {
        self.result = result;
        self
    }

    fn slots(&self, value: &Value) -> Vec<u32> {
        let Some(ty) = &self.result else {
            return slot(value).into_iter().collect();
        };
        let mut slots = Vec::new();
        let _ = values::lower_with(ty.clone(), value, &mut |_, value| {
            let id = slot(value).ok_or_else(|| crate::failure("resource result names no slot"))?;
            slots.push(id);
            // Only collect the slots: this temporary ABI value is never lowered.
            Ok(Val::Bool(false))
        });
        slots
    }

    /// Await the checked result; a refusal traps the importer, which quarantines its Store.
    async fn settle(&mut self) -> wasmtime::Result<Value> {
        self.answer()
            .await?
            .map_err(|message| wasmtime::Error::msg(format!("dependency call failed: {message}")))
    }

    /// Acquisition refusals are recoverable WIT errors; successful local slots keep the same
    /// abandoned-delivery guard as ordinary imported resources.
    async fn answer(&mut self) -> wasmtime::Result<std::result::Result<Value, String>> {
        let result = (&mut self.receive)
            .await
            .map_err(|_| wasmtime::Error::msg("host callback scope ended"))?;
        let text = match result {
            Ok(text) => text,
            Err(message) => return Ok(Err(message)),
        };
        let value: Value = serde_json::from_str(&text)?;
        if self.owned.is_some() {
            self.pending = self.slots(&value);
        }
        Ok(Ok(value))
    }

    /// The guest now holds the result.
    fn delivered(&mut self) {
        self.pending.clear();
        self.delivered = true;
    }
}

impl Drop for ImportReply {
    fn drop(&mut self) {
        let Some(dropped) = self.owned.as_ref().filter(|_| !self.delivered) else {
            return;
        };
        // Close before inspecting the buffer: a racing sender must either already have
        // published a slot we can drain, or receive a refusal and roll back its delivery.
        self.receive.close();
        // Received but never held, or accepted by the channel but never read.
        let mut slots = std::mem::take(&mut self.pending);
        if slots.is_empty()
            && let Ok(Ok(text)) = self.receive.try_recv()
            && let Ok(value) = serde_json::from_str(&text)
        {
            slots = self.slots(&value);
        }
        if let Ok(mut dropped) = dropped.lock() {
            dropped.extend(slots);
        }
    }
}

/// The importer table slot an owned import result names.
fn slot(value: &Value) -> Option<u32> {
    value.as_u64().and_then(|slot| u32::try_from(slot).ok())
}

/// Capture the issuing call's authority and publish one synchronous kernel state request for the
/// service object that call runs on.
fn state_request(
    state: &State,
    function: &str,
    params: &[Val],
) -> wasmtime::Result<oneshot::Receiver<std::result::Result<String, String>>> {
    let authority = issuing_authority(state, "kernel state has no admitted invocation")?;
    let Some(id) = authority.state else {
        return Err(wasmtime::Error::msg(
            if authority.environment == authority.scope {
                "contributions run in the Orb root, which owns no kernel state"
            } else {
                "this service object declares no kernel state"
            },
        ));
    };
    let packet = match (function, params) {
        ("read", []) => json!({"operation": "state.read", "id": id}),
        ("update", [Val::String(value)]) => {
            let mut value: Value = serde_json::from_str(value).map_err(|error| {
                wasmtime::Error::msg(format!("the state value is not JSON: {error}"))
            })?;
            unsigned_zeros(&mut value);
            json!({"operation": "state.update", "id": id, "value": value})
        }
        _ => return Err(wasmtime::Error::msg("unknown kernel state function")),
    }
    .to_string();
    synchronous(state, &authority, packet)
}

/// Write every negative zero of a component's state JSON as zero, as `JSON.stringify` does.
///
/// The portable value profile has no negative zero: a platform host would commit the value, then
/// refuse to encode it in its reply, and the object's state could never be read again.
fn unsigned_zeros(value: &mut Value) {
    match value {
        Value::Number(number) => {
            if number
                .as_f64()
                .is_some_and(|float| float == 0.0 && float.is_sign_negative())
            {
                *value = json!(0);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(unsigned_zeros),
        Value::Object(fields) => fields.values_mut().for_each(unsigned_zeros),
        Value::Null | Value::Bool(_) | Value::String(_) => {}
    }
}

/// The authority of the task group that issued the current import call.
fn issuing_authority(state: &State, missing: &str) -> wasmtime::Result<Authority> {
    let groups = state
        .groups
        .lock()
        .map_err(|_| wasmtime::Error::msg("task-group attribution is poisoned"))?;
    groups
        .current
        .and_then(|group| groups.bound.get(&group))
        .map(|bound| bound.authority.clone())
        .ok_or_else(|| wasmtime::Error::msg(missing.to_owned()))
}

/// Publish one bounded synchronous host request on the issuing call's channel.
fn synchronous(
    state: &State,
    authority: &Authority,
    packet: String,
) -> wasmtime::Result<oneshot::Receiver<std::result::Result<String, String>>> {
    if packet.len() > state.packet_bytes {
        return Err(wasmtime::Error::msg("host packet limit exceeded"));
    }
    let (reply, receive) = oneshot::channel();
    authority
        .events
        .try_send(Event::Host {
            packet,
            asynchronous: false,
            reply,
        })
        .map_err(|_| wasmtime::Error::msg("host operation queue is full or closed"))?;
    Ok(receive)
}

/// Capture the issuing call's authority and publish one `hook.run` for a hook this Orb declares:
/// the kernel runs the plan of contributions over the single input and answers with the result.
fn hook_request(
    state: &State,
    binding: &str,
    params: &[Val],
) -> wasmtime::Result<oneshot::Receiver<std::result::Result<String, String>>> {
    let authority = issuing_authority(state, "running a hook needs an admitted invocation")?;
    let Some(id) = authority.hooks.get(binding) else {
        return Err(wasmtime::Error::msg(format!(
            "the Orb root declares no hook for the import {binding}"
        )));
    };
    let [input] = params else {
        return Err(wasmtime::Error::msg("a hook callback takes one input"));
    };
    let value =
        values::lift(input.clone()).map_err(|error| wasmtime::Error::msg(error.to_string()))?;
    let packet = json!({
        "operation": "hook.run",
        "scope": authority.scope,
        "id": id,
        "value": value,
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

/// The composed result of a hook run; a refusal traps the caller.
async fn hook_settle(
    receive: oneshot::Receiver<std::result::Result<String, String>>,
) -> wasmtime::Result<Value> {
    let text = receive
        .await
        .map_err(|_| wasmtime::Error::msg("host callback scope ended"))?
        .map_err(|message| wasmtime::Error::msg(format!("hook run failed: {message}")))?;
    Ok(serde_json::from_str(&text)?)
}

/// How a kernel context function is answered.
enum ContextAnswer {
    /// Without the host: the object's declared address, or the log's empty answer.
    Local(Arc<Value>),
    /// By the host, on the issuing call's channel; the reply is lowered with the import's type.
    Host(oneshot::Receiver<std::result::Result<String, String>>),
}

/// Capture the issuing call's authority and answer one function of the kernel's context
/// interface: the object's address from the authority, the log through the session's
/// diagnostics, the call's principal and cancellation through the existing synchronous requests
/// of the call's channel.
fn context_request(
    state: &State,
    function: &str,
    params: &[Val],
) -> wasmtime::Result<ContextAnswer> {
    let authority = issuing_authority(state, "the kernel context needs an admitted invocation")?;
    let packet = match (function, params) {
        ("object", []) => {
            let address = authority.address.clone().ok_or_else(|| {
                wasmtime::Error::msg("the address of this environment's object was not declared")
            })?;
            return Ok(ContextAnswer::Local(address));
        }
        ("principal", []) => json!({"operation": "context.principal"}),
        ("cancelled", []) => json!({"operation": "signal.cancelled"}),
        ("cancel", []) => json!({"operation": "context.cancel"}),
        ("log", [Val::Enum(level), Val::String(message)]) => {
            // A diagnostic record is the host's, attributed to this revision, and never needs
            // the call's authority: the session's diagnostics take it, as they take a source
            // Orb's console records. Platform ports refuse `console` requests by design.
            let level = serde_json::from_value::<crate::protocol::LogLevel>(json!(level))
                .map_err(|_| wasmtime::Error::msg(format!("unknown log level {level}")))?;
            let answer = state
                .diagnostics
                .emit(level, message.clone())
                .map_err(|error| wasmtime::Error::msg(error.to_string()))?;
            return Ok(ContextAnswer::Local(Arc::new(answer)));
        }
        _ => return Err(wasmtime::Error::msg("unknown kernel context function")),
    }
    .to_string();
    Ok(ContextAnswer::Host(synchronous(state, &authority, packet)?))
}

/// Lower a context answer with the import's actual type; a refusal traps the caller.
async fn settle_context(
    answer: ContextAnswer,
    ty: &ComponentFunc,
    results: &mut [Val],
) -> wasmtime::Result<()> {
    let value = match answer {
        ContextAnswer::Local(address) => Value::clone(&address),
        ContextAnswer::Host(receive) => {
            let text = receive
                .await
                .map_err(|_| wasmtime::Error::msg("host callback scope ended"))?
                .map_err(|message| {
                    wasmtime::Error::msg(format!("kernel context refused: {message}"))
                })?;
            serde_json::from_str(&text)?
        }
    };
    store_result(results, lower_result(ty, &value)?)
}

/// Capture the issuing call's authority and publish one `resource.read` for a root resource the
/// component named; the kernel answers with the handle the component gave it when it opened.
fn resource_request(
    state: &State,
    function: &str,
    params: &[Val],
) -> wasmtime::Result<oneshot::Receiver<std::result::Result<String, String>>> {
    let authority = issuing_authority(state, "kernel resources have no admitted invocation")?;
    let ("read", [Val::String(name)]) = (function, params) else {
        return Err(wasmtime::Error::msg("unknown kernel resource function"));
    };
    let Some(id) = authority.resources.get(name.as_str()) else {
        return Err(wasmtime::Error::msg(format!(
            "the Orb root declares no resource named {name}"
        )));
    };
    let packet =
        json!({"operation": "resource.read", "scope": authority.scope, "id": id}).to_string();
    synchronous(state, &authority, packet)
}

/// Lower the handle of an open resource; a refusal (not ready, unknown) traps the caller.
async fn settle_resource(
    receive: oneshot::Receiver<std::result::Result<String, String>>,
    results: &mut [Val],
) -> wasmtime::Result<()> {
    let text = receive
        .await
        .map_err(|_| wasmtime::Error::msg("host callback scope ended"))?
        .map_err(|message| wasmtime::Error::msg(format!("kernel resource refused: {message}")))?;
    let handle: Value = serde_json::from_str(&text)?;
    let handle = handle
        .as_u64()
        .and_then(|handle| u32::try_from(handle).ok())
        .ok_or_else(|| wasmtime::Error::msg("resource handle is not a u32"))?;
    let Some(result) = results.first_mut() else {
        return Err(wasmtime::Error::msg(
            "kernel resource reads return the handle",
        ));
    };
    *result = Val::U32(handle);
    Ok(())
}

/// Lower the value the kernel retained as the JSON string result; a refusal traps the caller.
async fn settle_state(
    receive: oneshot::Receiver<std::result::Result<String, String>>,
    results: &mut [Val],
) -> wasmtime::Result<()> {
    let text = receive
        .await
        .map_err(|_| wasmtime::Error::msg("host callback scope ended"))?
        .map_err(|message| wasmtime::Error::msg(format!("kernel state refused: {message}")))?;
    let snapshot: Value = serde_json::from_str(&text)?;
    let Some(result) = results.first_mut() else {
        return Err(wasmtime::Error::msg(
            "kernel state functions return the retained value",
        ));
    };
    *result = Val::String(snapshot["value"].to_string());
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
        Packet::Create { .. } | Packet::Validate { .. } => Operation::Declare,
        Packet::Invoke { .. }
        | Packet::Contribute { .. }
        | Packet::Release { .. }
        | Packet::Destroy { .. }
        | Packet::Ingress { .. }
        | Packet::Callback { .. } => Operation::Start,
        Packet::Unsupported => return Ok(Err("unknown compiled command".into())),
    };
    if command.operation != expected {
        return Ok(Err("command does not match its execution entrypoint".into()));
    }
    let limit = engine.limits.packet_bytes;
    match packet {
        Packet::Create {
            environment,
            service,
        } => match declare(
            environment,
            service.clone(),
            layout,
            environments,
            Runtime {
                engine,
                store,
                exports,
                groups,
            },
            &command.events,
            limit,
        )
        .await
        {
            Ok(()) => {
                environments.services.insert(environment, service);
                Ok(Ok("null".to_owned()))
            }
            Err(Failure::Refused(message)) => Ok(Err(message)),
            Err(Failure::Trap(error)) => Err(error),
        },
        Packet::Release { environment } => {
            environments.services.remove(&environment);
            environments.roots.remove(&environment);
            let root = environments.parents.remove(&environment);
            environments.states.remove(&environment);
            environments.resources.remove(&environment);
            environments.hooks.remove(&environment);
            environments.addresses.remove(&environment);
            environments
                .callbacks
                .retain(|_, callback| callback.environment() != environment);
            // Resources this environment issued are destroyed with it; entries leave the table
            // first, so no owner can reach them afterwards.
            let resources = store.data_mut().instances.take(environment);
            super::resources::destroy(engine, store, resources).await?;
            // Imports its destructors dropped belong to the Orb root, which outlives a service
            // environment; a released root takes its whole table with it.
            if let Some(root) = root {
                let dropped = store.data().dropped.clone();
                release_dropped(&dropped, &(command.events.clone(), root)).await;
            }
            Ok(Ok("null".into()))
        }
        Packet::Destroy {
            environment,
            instance,
        } => {
            let resources = store
                .data_mut()
                .instances
                .remove(environment, instance)
                .into_iter()
                .collect();
            super::resources::destroy(engine, store, resources).await?;
            // Imports the destructor dropped are released on this command's channel when the
            // host awaits it; otherwise they stay queued for the next call of this Store.
            let root = environments
                .parents
                .get(&environment)
                .copied()
                .unwrap_or(environment);
            let dropped = store.data().dropped.clone();
            release_dropped(&dropped, &(command.events.clone(), root)).await;
            Ok(Ok("null".into()))
        }
        Packet::Validate { callback, value } => {
            let Some(Callback::State {
                environment,
                interface,
            }) = environments.callbacks.get(&callback).cloned()
            else {
                return Ok(Err("unknown compiled state validator".into()));
            };
            let Some(function) = exports
                .kernel
                .get(&(guest::STATE_INIT.to_owned(), guest::VALIDATE.to_owned()))
                .copied()
            else {
                return Ok(Err("component declares no kernel state validator".into()));
            };
            // The validator runs under the declaration's channel, where the kernel answers only
            // reads of the retained state and the log.
            let authority = match authority(environment, layout, environments, command) {
                Ok(authority) => authority,
                Err(error) => return Ok(Err(error)),
            };
            let call = Call {
                function,
                roles: Resolved::default(),
                environment,
                receiver: None,
                stream: None,
                arguments: None,
            };
            let args = json!([interface, value.to_string()]);
            let verdict = match invoke(engine, store, call, &args, authority, groups).await? {
                Ok(verdict) => verdict,
                Err(error) => return Ok(Err(error)),
            };
            let verdict: Value = serde_json::from_str(&verdict)?;
            Ok(match verdict["tag"].as_str() {
                Some("ok") => Ok("null".into()),
                Some("err") => Err(verdict["val"]
                    .as_str()
                    .map(|message| format!("the state was refused: {message}"))
                    .unwrap_or_else(|| "the state was refused".into())),
                _ => Err("the state validator answered no result".into()),
            })
        }
        Packet::Callback { callback, value } => {
            let Some(Callback::Resource {
                environment,
                name,
                open,
            }) = environments.callbacks.get(&callback).cloned()
            else {
                return Ok(Err("unknown compiled resource callback".into()));
            };
            let function = if open { guest::OPEN } else { guest::CLOSE };
            let Some(function) = exports
                .kernel
                .get(&(guest::RESOURCE_INIT.to_owned(), function.to_owned()))
                .copied()
            else {
                return Ok(Err("component declares no kernel resource callbacks".into()));
            };
            let args = match (open, value) {
                (true, _) => json!([name]),
                (false, Some(handle)) => json!([name, handle]),
                (false, None) => return Ok(Err("resource closer without a handle".into())),
            };
            let authority = Authority {
                events: command.events.clone(),
                scope: environment,
                handles: environments
                    .roots
                    .get(&environment)
                    .cloned()
                    .unwrap_or_default(),
                environment,
                state: None,
                resources: environments
                    .resources
                    .get(&environment)
                    .cloned()
                    .unwrap_or_default(),
                hooks: environments
                    .hooks
                    .get(&environment)
                    .cloned()
                    .unwrap_or_default(),
                address: environments.addresses.get(&environment).cloned(),
            };
            let call = Call {
                function,
                roles: Resolved::default(),
                environment,
                receiver: None,
                stream: None,
                arguments: None,
            };
            invoke(engine, store, call, &args, authority, groups).await
        }
        Packet::Invoke {
            environment,
            function,
            args,
            receiver,
        } if command.ingress.is_none() => {
            // A service method, called only in an environment of its own service.
            let target = match exports.functions.get(&function) {
                Some(target) if !layout.contributions.contains(&function) => target,
                _ => return Ok(Err("unknown component export".into())),
            };
            let Some(service) = environments.services.get(&environment) else {
                return Ok(Err(
                    "component environment is not declared or was released".into()
                ));
            };
            if service.as_deref() != Some(&*target.service) {
                return Ok(Err(
                    "the function does not belong to this environment's service".into(),
                ));
            }
            let target = target.clone();
            let authority = match authority(environment, layout, environments, command) {
                Ok(authority) => authority,
                Err(error) => return Ok(Err(error)),
            };
            let call = Call {
                function: target.function,
                roles: target.roles,
                environment,
                receiver,
                stream: command.stream.take(),
                arguments: command.arguments.take(),
            };
            invoke(engine, store, call, &args, authority, groups).await
        }
        Packet::Contribute {
            environment,
            function,
            args,
        } if command.ingress.is_none() && command.stream.is_none() => {
            // A contribution callback, called only in its Orb root environment.
            let target = match exports.functions.get(&function) {
                Some(target) if layout.contributions.contains(&function) => target.clone(),
                _ => return Ok(Err("unknown compiled contribution callback".into())),
            };
            if environments.services.get(&environment) != Some(&None) {
                return Ok(Err(
                    "a contribution runs only in its declared Orb root environment".into(),
                ));
            }
            let authority = match authority(environment, layout, environments, command) {
                Ok(authority) => authority,
                Err(error) => return Ok(Err(error)),
            };
            let call = Call {
                function: target.function,
                roles: target.roles,
                environment,
                receiver: None,
                stream: None,
                arguments: None,
            };
            invoke(engine, store, call, &args, authority, groups).await
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
        environment,
        state: environments.states.get(&environment).copied(),
        resources: environments
            .resources
            .get(&root)
            .cloned()
            .unwrap_or_default(),
        hooks: environments.hooks.get(&root).cloned().unwrap_or_default(),
        address: environments.addresses.get(&environment).cloned(),
    })
}

/// A declaration refused by the host refuses only this object; a trap retires the Store.
enum Failure {
    Refused(String),
    Trap(wasmtime::Error),
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self::Refused(message)
    }
}

impl From<&str> for Failure {
    fn from(message: &str) -> Self {
        Self::Refused(message.to_owned())
    }
}

impl From<wasmtime::Error> for Failure {
    fn from(error: wasmtime::Error) -> Self {
        Self::Trap(error)
    }
}

/// The Store and its exports, lent to a declaration that calls back into the component.
struct Runtime<'a> {
    engine: &'a Engine,
    store: &'a mut Store<State>,
    exports: &'a Exports,
    groups: &'a Arc<Mutex<Groups>>,
}

/// Declare one environment. An Orb root acquires default-key references for its dependency
/// imports; a service object records its root and, when the component owns kernel state, asks
/// the component for this service's declaration and declares it in the object's own table.
async fn declare(
    environment: u64,
    service: Option<String>,
    layout: &Layout,
    environments: &mut Environments,
    runtime: Runtime<'_>,
    events: &Sender<Event>,
    limit: usize,
) -> std::result::Result<(), Failure> {
    let Runtime {
        engine,
        store,
        exports,
        groups,
    } = runtime;
    if layout.imports.is_empty() && !layout.state_init && !layout.resource_init {
        return Ok(());
    }
    // The address of this environment's object, which the kernel tells only while the
    // environment is declared; the context interface answers `object` from it.
    if layout.context {
        let address = declaration(events, json!({"operation": "address"}), limit).await?;
        environments
            .addresses
            .insert(environment, Arc::new(address));
    }
    let Some(alias) = service else {
        let mut handles = BTreeMap::new();
        for import in &layout.imports {
            // A reference view acquires its objects by key and declares no default `main`.
            if import.role != Role::Dependency || import.reference {
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
        let handles = Arc::new(handles);
        environments.roots.insert(environment, handles.clone());
        // The hooks this Orb declares are bound in the root's table, as a source provider binds
        // them in its factory; the import of each runs the plan through that handle.
        let mut hooks = BTreeMap::new();
        for (binding, alias) in &layout.hooks {
            let id = declaration(
                events,
                json!({"operation": "hook.declare", "name": alias}),
                limit,
            )
            .await?;
            let id = id
                .as_u64()
                .and_then(|id| u32::try_from(id).ok())
                .ok_or("invalid hook handle")?;
            hooks.insert(binding.clone(), id);
        }
        let hooks = Arc::new(hooks);
        environments.hooks.insert(environment, hooks.clone());
        if !layout.resource_init {
            return Ok(());
        }
        // The root's resources: the component lists them, the worker numbers their callbacks
        // and declares each one; the kernel opens them during activation through `callback`.
        let function = exports
            .kernel
            .get(&(guest::RESOURCE_INIT.to_owned(), guest::DECLARE.to_owned()))
            .copied()
            .ok_or("component declares no kernel resource callbacks")?;
        let authority = Authority {
            events: events.clone(),
            scope: environment,
            handles,
            environment,
            state: None,
            resources: Arc::default(),
            hooks,
            address: environments.addresses.get(&environment).cloned(),
        };
        let call = Call {
            function,
            roles: Resolved::default(),
            environment,
            receiver: None,
            stream: None,
            arguments: None,
        };
        let answer = invoke(engine, store, call, &json!([]), authority, groups).await??;
        let declared: Value = serde_json::from_str(&answer).map_err(|error| error.to_string())?;
        let declared = declared
            .as_array()
            .ok_or("the resource declarations are not a list")?;
        let mut resources = BTreeMap::new();
        for resource in declared {
            let name = resource["name"]
                .as_str()
                .ok_or("a resource declaration carries no name")?
                .to_owned();
            let version = resource["version"]
                .as_u64()
                .ok_or("a resource declaration carries no version")?;
            let (open, close) = (environments.callback()?, environments.callback()?);
            let id = declaration(
                events,
                json!({
                    "operation": "resource.declare",
                    "name": name,
                    "version": version,
                    "open": open,
                    "close": close,
                    "dependencies": [],
                }),
                limit,
            )
            .await?;
            let id = id
                .as_u64()
                .and_then(|id| u32::try_from(id).ok())
                .ok_or("invalid resource handle")?;
            for (callback, opens) in [(open, true), (close, false)] {
                environments.callbacks.insert(
                    callback,
                    Callback::Resource {
                        environment,
                        name: name.clone(),
                        open: opens,
                    },
                );
            }
            if resources.insert(name, id).is_some() {
                return Err("a resource is declared twice".into());
            }
        }
        environments
            .resources
            .insert(environment, Arc::new(resources));
        return Ok(());
    };
    let root = declaration(events, json!({"operation": "orb"}), limit).await?;
    let root = root
        .as_u64()
        .filter(|root| environments.roots.contains_key(root))
        .ok_or("service environment has no declared Orb root")?;
    environments.parents.insert(environment, root);
    if !layout.state_init {
        return Ok(());
    }
    // Only the objects of a service the component exports own kernel state. The incoming HTTP
    // handler is created as a service of its own but answers no interface of the component, so
    // its objects declare none, as in the browser's component kernel.
    let Some(interface) = layout
        .exports
        .iter()
        .find(|export| export.service == alias)
        .map(|export| export.interface.clone())
    else {
        return Ok(());
    };
    let function = exports
        .kernel
        .get(&(guest::STATE_INIT.to_owned(), guest::DECLARE.to_owned()))
        .copied()
        .ok_or("component declares no kernel state-init callback")?;
    // The callback runs under the declaration's channel; it may not read state (none exists yet).
    let authority = Authority {
        events: events.clone(),
        scope: root,
        handles: environments.roots.get(&root).cloned().unwrap_or_default(),
        environment,
        state: None,
        resources: environments
            .resources
            .get(&root)
            .cloned()
            .unwrap_or_default(),
        hooks: environments.hooks.get(&root).cloned().unwrap_or_default(),
        address: environments.addresses.get(&environment).cloned(),
    };
    let call = Call {
        function,
        roles: Resolved::default(),
        environment,
        receiver: None,
        stream: None,
        arguments: None,
    };
    let answer = invoke(engine, store, call, &json!([interface]), authority, groups).await??;
    let declared: Value = serde_json::from_str(&answer).map_err(|error| error.to_string())?;
    if declared.is_null() {
        return Ok(());
    }
    let initial = declared["initial"]
        .as_str()
        .ok_or("the state declaration carries no initial value")?;
    let mut initial: Value = serde_json::from_str(initial)
        .map_err(|error| format!("the initial state is not JSON: {error}"))?;
    unsigned_zeros(&mut initial);
    let version = declared["version"]
        .as_u64()
        .ok_or("the state declaration carries no version")?;
    // A validated state gets a worker-numbered callback; the kernel runs it, through a
    // `validate` command of this Store, on the value a reload would hand to this instance.
    let validate = match declared["validated"].as_bool() {
        Some(true) => {
            let callback = environments.callback()?;
            environments.callbacks.insert(
                callback,
                Callback::State {
                    environment,
                    interface,
                },
            );
            Some(callback)
        }
        Some(false) => None,
        None => return Err("the state declaration does not say whether it is validated".into()),
    };
    let id = declaration(
        events,
        json!({"operation": "state.declare", "initial": initial, "version": version, "validate": validate}),
        limit,
    )
    .await?;
    let id = id
        .as_u64()
        .and_then(|id| u32::try_from(id).ok())
        .ok_or("invalid state handle")?;
    environments.states.insert(environment, id);
    Ok(())
}

/// One export call and the exported resource it lends as implicit `self`, if any.
struct Call {
    function: Func,
    roles: Resolved,
    environment: u64,
    receiver: Option<u32>,
    /// The host-created end of a whole byte-stream result.
    stream: Option<StreamCall>,
    arguments: Option<crate::worker::arguments::Arguments>,
}

/// Run one export as a staged concurrent call and settle it only after its task group finishes.
async fn invoke(
    engine: &Engine,
    store: &mut Store<State>,
    call: Call,
    arguments: &Value,
    authority: Authority,
    groups: &Arc<Mutex<Groups>>,
) -> wasmtime::Result<std::result::Result<String, String>> {
    let Call {
        function,
        roles,
        environment,
        receiver,
        stream,
        arguments: ownership,
    } = call;
    let ty = function.ty(&*store);
    let Some(args) = arguments.as_array() else {
        return Ok(Err("component arguments require a tuple".into()));
    };
    let lent = usize::from(roles.receiver.is_some());
    if args.len() + lent != ty.params().len() || ty.results().len() > 1 {
        return Ok(Err("component function arity differs".into()));
    }
    let mut lowered = Vec::with_capacity(ty.params().len());
    // The lent resource must still be held for this environment with exactly this type:
    // lowering a destroyed handle would fail every call of the Store, not just this one.
    match (&roles.receiver, receiver) {
        (Some(expected), Some(id)) => {
            match store.data().instances.receiver(environment, id, expected) {
                Some(resource) => lowered.push(Val::Resource(resource)),
                None => return Ok(Err("the resource instance is gone".into())),
            }
        }
        (None, None) => {}
        _ => return Ok(Err("the call's receiver differs from its export".into())),
    }
    let prepared = match arguments::lower(store, &ty, args, lent, ownership)? {
        Ok(prepared) => prepared,
        Err(error) => return Ok(Err(error)),
    };
    lowered.extend(prepared.values);
    let accepted = prepared.accepted;
    let args = lowered;
    let count = ty.results().len();
    let owned_results = roles.owned_results;
    let releases = (authority.events.clone(), authority.scope);
    let dropped = store.data().dropped.clone();
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
    let stream_pipe = stream.as_ref().map(|stream| stream.pipe.clone());
    let call = store.run_concurrent(async move |accessor: &Accessor<State>| {
        let work = async {
            let mut results = vec![Val::Bool(false); count];
            lock(&groups)?.pending = Some(Bound {
                authority,
                finished: Some(finished),
                call_work: call_work.clone(),
                cancelled,
            });
            let staged = accessor.with(|mut access| {
                function.start_call_concurrent(&mut access, &args, &mut results)
            });
            let staged = match staged {
                Ok(staged) => staged,
                Err(error) => {
                    lock(&groups)?.pending = None;
                    return Err(error);
                }
            };
            if let Some(accepted) = accepted {
                accepted
                    .send(())
                    .map_err(|_| wasmtime::Error::msg("owned argument admission was abandoned"))?;
            }
            let group = staged.group();
            {
                let groups = lock(&groups)?;
                if groups.pending.is_some() || !groups.bound.contains_key(&group) {
                    return Err(wasmtime::Error::msg(
                        "component call authority was not bound to its task group",
                    ));
                }
            }
            function.finish_call_concurrent(accessor, staged).await?;
            // A stream result is published now, as the head; its producer keeps running in this
            // call's task group.
            let consumer = match stream {
                Some(stream) => Some(publish(accessor, results.pop(), stream)?),
                None => None,
            };
            // A published result is not physical completion: wait until every task and
            // subtask of this call has exited, so no continuation can outlive its authority.
            settled.await.map_err(|_| {
                wasmtime::Error::msg("component task group ended without finishing")
            })?;
            // A stream's call also ends only once its consumer is gone: the guest dropped its
            // writer, or the reader closed and the writer saw it.
            if let Some(consumer) = consumer {
                let _ = consumer.await;
            }
            Ok::<_, wasmtime::Error>(results)
        };
        let results = {
            let mut work = std::pin::pin!(work);
            // The caller's cancellation closes the bodies this call is reading at once, so a
            // guest parked in a body read gets its end and returns; the call finishes normally.
            // A stream it writes is failed, so its next write is dropped and no reader sees an end.
            match futures::future::select(work.as_mut(), watch).await {
                futures::future::Either::Left((done, _)) => done?,
                futures::future::Either::Right(((), _)) => {
                    commands.cancel();
                    if let Some(pipe) = &stream_pipe {
                        pipe.lock()
                            .expect("pipe")
                            .fail("the stream's call was cancelled".into());
                    }
                    work.await?
                }
            }
        };
        // Nor is a finished group: bodies it read close now, and a cancelled send may still own
        // a resolver lookup or a reactor connect. Release the call after they physically ended.
        commands.end();
        call_work.settled().await;
        // Socket streams the guest dropped during this call close physically before it ends,
        // and accepted sockets nobody will read are closed through upstream.
        closing.settled().await;
        accessor.with(|mut store| crate::worker::stream_adapter::release(store.data_mut()))?;
        crate::worker::stream_adapter::sweep(accessor).await?;
        // Imported resources the guest dropped are released before the call completes.
        release_dropped(&dropped, &releases).await;
        // Every owned result stays in this Store, including handles nested in data containers.
        let value = match results.into_iter().next() {
            Some(value) if owned_results => {
                keep(accessor, value, environment, engine.limits.packet_bytes).await?
            }
            Some(value) => values::lift(value).map_err(|error| error.to_string()),
            None => Ok(Value::Null),
        };
        Ok::<_, wasmtime::Error>(value)
    });
    let value = tokio::time::timeout(
        std::time::Duration::from_millis(engine.limits.timeout_ms),
        call,
    )
    .await
    .map_err(|_| wasmtime::Error::msg("component call timed out"))???;
    let value = match value {
        Ok(value) => value,
        Err(error) => return Ok(Err(error)),
    };
    let text = value.to_string();
    if text.len() > engine.limits.packet_bytes {
        return Ok(Err("component result exceeds the packet limit".into()));
    }
    Ok(Ok(text))
}

/// Publish a whole `stream<u8>` result: convert it exactly once, pipe it into the host-created
/// pipe and signal the head. The returned receiver resolves once the pipe's consumer is gone.
fn publish(
    accessor: &Accessor<State>,
    result: Option<Val>,
    stream: StreamCall,
) -> wasmtime::Result<oneshot::Receiver<()>> {
    let Some(Val::Stream(result)) = result else {
        return Err(wasmtime::Error::msg("export returned no stream"));
    };
    let reader = result.try_into_stream_reader::<u8>()?;
    let (gone, consumer_gone) = oneshot::channel();
    accessor.with(|mut access| {
        reader.pipe(
            &mut access,
            Carried {
                consumer: PipeConsumer::new(stream.pipe),
                _gone: gone,
            },
        )
    })?;
    let _ = stream.head.send(());
    Ok(consumer_gone)
}

/// A stream result's consumer; dropping it ends the pipe first, then reports that it is gone.
struct Carried {
    consumer: PipeConsumer,
    _gone: oneshot::Sender<()>,
}

impl wasmtime::component::StreamConsumer<State> for Carried {
    type Item = u8;

    fn poll_consume(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        store: wasmtime::StoreContextMut<'_, State>,
        source: wasmtime::component::Source<'_, u8>,
        finish: bool,
    ) -> std::task::Poll<wasmtime::Result<wasmtime::component::StreamResult>> {
        std::pin::Pin::new(&mut self.get_mut().consumer)
            .poll_consume(context, store, source, finish)
    }
}

/// File every owned resource nested in a result, rolling back the complete batch on failure.
async fn keep(
    accessor: &Accessor<State>,
    value: Val,
    environment: u64,
    packet_bytes: usize,
) -> wasmtime::Result<std::result::Result<Value, String>> {
    let mut resources = Vec::new();
    values::resources(&value, &mut |resource| resources.push(resource));
    let checked = values::lift_with(value.clone(), &mut |resource| {
        if !resource.owned() {
            return Err(crate::failure("a result cannot return a borrowed resource"));
        }
        Ok(json!(0))
    });
    let mut inserted = Vec::new();
    let lifted = checked.and_then(|_| {
        values::lift_with(value, &mut |resource| {
            let instance = accessor
                .with(|mut store| store.data_mut().instances.insert(environment, resource))
                .map_err(crate::failure)?;
            inserted.push(instance);
            Ok(json!(instance))
        })
    });
    let lifted = lifted.and_then(|value| {
        if value.to_string().len() > packet_bytes {
            return Err(crate::failure("component result exceeds the packet limit"));
        }
        Ok(value)
    });
    match lifted {
        Ok(value) => Ok(Ok(value)),
        Err(error) => {
            accessor.with(|mut store| {
                for id in inserted {
                    store.data_mut().instances.remove(environment, id);
                }
            });
            for resource in resources {
                if resource.owned() {
                    resource.resource_drop_concurrent(accessor).await?;
                }
            }
            Ok(Err(error.to_string()))
        }
    }
}

/// Release the imported resource slots the guest dropped, on `events`.
///
/// A refused release leaves the slot to its root environment's teardown; it never fails the
/// Store. If the channel is gone, the slots not yet released stay queued for the next one.
async fn release_dropped(dropped: &Dropped, (events, scope): &(Sender<Event>, u64)) {
    let slots = dropped
        .lock()
        .map(|mut queue| std::mem::take(&mut *queue))
        .unwrap_or_default();
    for (index, slot) in slots.iter().enumerate() {
        let (reply, receive) = oneshot::channel();
        let packet = json!({"operation": "reference.release", "scope": scope, "id": slot});
        let sent = events
            .send(Event::Host {
                packet: packet.to_string(),
                asynchronous: false,
                reply,
            })
            .await;
        if sent.is_err() {
            if let Ok(mut queue) = dropped.lock() {
                let later = std::mem::take(&mut *queue);
                queue.extend_from_slice(&slots[index..]);
                queue.extend(later);
            }
            return;
        }
        let _ = receive.await;
    }
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
    let releases = (authority.events.clone(), authority.scope);
    let refusals = authority.events.clone();
    let dropped = store.data().dropped.clone();
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
                    // Typed for platform carriers, on the call's own channel before its `Done`.
                    let refusal = crate::ingress::IngressEvent::refused(code);
                    let _ = refusals.send(Event::Ingress(refusal)).await;
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
        // Imported resources the handler dropped are released before the request completes.
        release_dropped(&dropped, &releases).await;
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

#[cfg(test)]
mod import_reply_tests {
    use super::{Dropped, ImportReply};
    use tokio::sync::oneshot;

    fn queued(dropped: &Dropped) -> Vec<u32> {
        dropped.lock().unwrap().clone()
    }

    /// The host committed slot 7: the reply reached this Store's channel.
    fn committed() -> (ImportReply, Dropped) {
        let dropped = Dropped::default();
        let (reply, receive) = oneshot::channel();
        reply.send(Ok("7".into())).unwrap();
        (ImportReply::new(receive, Some(dropped.clone())), dropped)
    }

    #[test]
    fn an_abandoned_committed_reply_queues_its_slot() {
        let (reply, dropped) = committed();
        drop(reply);
        assert_eq!(queued(&dropped), [7]);
    }

    #[tokio::test]
    async fn a_reply_read_but_never_held_queues_its_slot() {
        let (mut reply, dropped) = committed();
        assert_eq!(reply.settle().await.unwrap(), 7);
        drop(reply);
        assert_eq!(queued(&dropped), [7]);
    }

    #[tokio::test]
    async fn a_delivered_reply_queues_nothing() {
        let (mut reply, dropped) = committed();
        reply.settle().await.unwrap();
        reply.delivered();
        drop(reply);
        assert!(queued(&dropped).is_empty());
    }

    #[test]
    fn data_replies_and_unanswered_calls_queue_nothing() {
        let dropped = Dropped::default();
        let (reply, receive) = oneshot::channel::<Result<String, String>>();
        reply.send(Ok("7".into())).unwrap();
        drop(ImportReply::new(receive, None));
        let (_reply, receive) = oneshot::channel();
        drop(ImportReply::new(receive, Some(dropped.clone())));
        assert!(queued(&dropped).is_empty());
    }

    #[test]
    fn a_racing_delivery_is_either_refused_or_queued_for_release() {
        use std::sync::{Arc, Barrier, mpsc};
        let barrier = Arc::new(Barrier::new(2));
        let (publish, requests) = mpsc::channel::<oneshot::Sender<Result<String, String>>>();
        let (sent, accepted) = mpsc::channel();
        std::thread::scope(|threads| {
            let ready = barrier.clone();
            threads.spawn(move || {
                for sender in requests {
                    ready.wait();
                    sent.send(sender.send(Ok("7".into())).is_ok()).unwrap();
                }
            });
            for _ in 0..256 {
                let dropped = Dropped::default();
                let (sender, receive) = oneshot::channel();
                let reply = ImportReply::new(receive, Some(dropped.clone()));
                publish.send(sender).unwrap();
                barrier.wait();
                drop(reply);
                let delivered = accepted.recv().unwrap();
                assert_eq!(queued(&dropped), if delivered { vec![7] } else { vec![] });
            }
            drop(publish);
        });
    }
}

#[cfg(test)]
mod state_value_tests {
    use super::unsigned_zeros;
    use serde_json::json;

    /// A state field that rounds to negative zero is stored as zero, wherever it sits, and no
    /// other value changes: the platform hosts' value profile has no negative zero.
    #[test]
    fn negative_zeros_of_a_state_become_zero() {
        let mut value: serde_json::Value =
            serde_json::from_str(r#"{"x":-0.0,"y":[-0,0.5,-1.5],"z":{"w":-0.0},"s":"text"}"#)
                .unwrap();
        unsigned_zeros(&mut value);
        assert_eq!(
            value,
            json!({"x": 0, "y": [0, 0.5, -1.5], "z": {"w": 0}, "s": "text"})
        );
        assert!(!value.to_string().contains("-0"), "{value}");
    }
}
