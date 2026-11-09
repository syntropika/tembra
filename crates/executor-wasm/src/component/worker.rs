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
        Accessor, Component, Func, Linker, ResourceTable, TaskGroupHook, TaskGroupId, Val,
        types::ComponentFunc,
    },
};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

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
}

/// Task-group attribution shared between the Store's hook and its import functions.
#[derive(Default)]
struct Groups {
    /// Group of the thread the runtime has entered, if any.
    current: Option<TaskGroupId>,
    /// Authority waiting for the next staged call's group to start.
    pending: Option<Bound>,
    bound: HashMap<TaskGroupId, Bound>,
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

struct State {
    wasi: WasiCtx,
    table: ResourceTable,
    limits: StoreLimits,
    groups: Arc<Mutex<Groups>>,
    packet_bytes: usize,
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
    wasi: WasiCtxBuilder,
    layout: Layout,
    ready: mpsc::Sender<std::result::Result<(), String>>,
    commands: mpsc::Receiver<Command>,
) -> Terminal {
    let groups = Arc::new(Mutex::new(Groups::default()));
    let (mut store, exports) = match instantiate(&engine, &component, wasi, &layout, &groups).await
    {
        Ok(instance) => instance,
        Err(error) => {
            let _ = ready.send(Err(format!("component initialization failed: {error:#}")));
            return Vec::new();
        }
    };
    let _ = ready.send(Ok(()));
    let mut environments = Environments::default();
    while let Ok(command) = commands.recv() {
        let served = serve(
            &engine,
            &mut store,
            &exports,
            &layout,
            &mut environments,
            &groups,
            &command,
        )
        .await;
        match served {
            Ok(result) => {
                let _ = command.events.send(Event::Done(result)).await;
            }
            Err(error) => {
                // The Store is dropped with this future; its completion waits for runtime shutdown.
                return vec![(command.events, format!("component trap: {error:#}"))];
            }
        }
    }
    Vec::new()
}

/// Link granted WASI and the selected Orb imports, then instantiate within the callback bound.
async fn instantiate(
    engine: &Engine,
    component: &Component,
    mut wasi: WasiCtxBuilder,
    layout: &Layout,
    groups: &Arc<Mutex<Groups>>,
) -> wasmtime::Result<(Store<State>, BTreeMap<(String, String), Func>)> {
    let mut linker = Linker::<State>::new(&engine.engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    link_imports(&mut linker, layout)?;
    let mut store = Store::new(
        &engine.engine,
        State {
            wasi: wasi.build(),
            table: ResourceTable::new(),
            limits: StoreLimitsBuilder::new()
                .memory_size(engine.limits.memory_bytes)
                .build(),
            groups: groups.clone(),
            packet_bytes: engine.limits.packet_bytes,
        },
    );
    store.limiter(|state| &mut state.limits);
    store.set_fuel(engine.limits.fuel)?;
    store.fuel_async_yield_interval(Some(10_000))?;
    store.task_group_hook(Hook(groups.clone()));
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
    Ok((store, exports))
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
    exports: &BTreeMap<(String, String), Func>,
    layout: &Layout,
    environments: &mut Environments,
    groups: &Arc<Mutex<Groups>>,
    command: &Command,
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
        Packet::Invoke { .. } | Packet::Contribute { .. } | Packet::Release { .. } => {
            Operation::Start
        }
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
        } if command.operation == Operation::Start => {
            let Some(function) = exports.get(&(service, method)).copied() else {
                return Ok(Err("unknown component export".into()));
            };
            let root = environments
                .parents
                .get(&environment)
                .copied()
                .unwrap_or(environment);
            let handles = match (environments.roots.get(&root), layout.imports.is_empty()) {
                (Some(handles), _) => handles.clone(),
                (None, true) => Arc::default(),
                (None, false) => return Ok(Err("component environment is not declared".into())),
            };
            let authority = Authority {
                events: command.events.clone(),
                scope: root,
                handles,
            };
            invoke(engine, store, function, &args, authority, groups).await
        }
        Packet::Invoke { .. } | Packet::Contribute { .. } | Packet::Unsupported => Ok(Err(
            "compiled components have no guest callbacks of this kind".into(),
        )),
    }
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
    let groups = groups.clone();
    let call = store.run_concurrent(async move |accessor: &Accessor<State>| {
        let mut results = vec![Val::Bool(false); count];
        lock(&groups)?.pending = Some(Bound {
            authority,
            finished: Some(finished),
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
        function.finish_call_concurrent(accessor, staged).await?;
        // A published result is not physical completion: wait until every task and subtask of
        // this call has exited, so no continuation can outlive the call's authority.
        settled
            .await
            .map_err(|_| wasmtime::Error::msg("component task group ended without finishing"))?;
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

fn lock(groups: &Mutex<Groups>) -> wasmtime::Result<std::sync::MutexGuard<'_, Groups>> {
    groups
        .lock()
        .map_err(|_| wasmtime::Error::msg("task-group attribution is poisoned"))
}
