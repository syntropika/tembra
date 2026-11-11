//! Prepared source Orbs on the component worker.
//!
//! A source Orb is a component built from JavaScript against the private `tembra:orb` package.
//! This module links that package's typed host imports, the one-turn reference and hook
//! selections, the self-imports through which the Orb runs its own hooks and the runtime's fixed
//! WASI 0.2 internals. Every host request travels on the channel of the callback whose task group
//! issued it, exactly like a compiled dependency import; a selection never grants authority.
use super::worker::{State, host_request};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::oneshot;
use wasmtime::component::{
    Accessor, ComponentType, Func, Instance, Lift, Linker, Lower, Val, types::ComponentItem,
};

type Reply = oneshot::Receiver<std::result::Result<String, String>>;

/// A host-issued handle, optionally qualified by the environment that issued it.
#[derive(Clone, Copy, Debug, ComponentType, Lift, Lower)]
#[component(record)]
pub(crate) struct Handle {
    pub scope: Option<u64>,
    pub id: u32,
}

// Variants are constructed only by lifting guest values.
#[allow(dead_code)]
#[derive(Clone, Copy, ComponentType, Lift)]
#[component(enum)]
#[repr(u8)]
enum Level {
    #[component(name = "log")]
    Log,
    #[component(name = "debug")]
    Debug,
    #[component(name = "info")]
    Info,
    #[component(name = "warn")]
    Warn,
    #[component(name = "error")]
    Error,
}

/// A kernel snapshot reply of the private host protocol.
#[derive(serde::Deserialize)]
struct Snapshot {
    #[serde(default)]
    value: Option<Value>,
    revision: String,
    version: u32,
}

/// The guest-visible snapshot: kernel state stays JSON text.
#[derive(ComponentType, Lower)]
#[component(record)]
struct SnapshotText {
    value: Option<String>,
    revision: String,
    version: u32,
}

#[derive(ComponentType, Lower, serde::Deserialize)]
#[component(record)]
struct Address {
    key: Vec<String>,
    principal: String,
}

/// The pending one-turn selection of this Store. JavaScript runs one turn at a time, and the
/// selected call is issued in the same turn, so one slot per Store is exact.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Selection {
    Reference(Handle),
    Hook(Handle),
}

/// Take the pending reference selection for a dependency import of `binding`.
///
/// Source Orbs have no default dependency object: an import without a selection is refused.
pub(crate) fn take_reference(state: &mut State) -> wasmtime::Result<Handle> {
    match state.selection.take() {
        Some(Selection::Reference(handle)) => Ok(handle),
        Some(Selection::Hook(_)) => Err(wasmtime::Error::msg(
            "a hook selection was followed by a dependency import",
        )),
        None => Err(wasmtime::Error::msg(
            "a source Orb dependency call requires a selected reference",
        )),
    }
}

/// Decode a host reply into the guest's `result<T, string>` value.
fn decode<T: serde::de::DeserializeOwned>(
    reply: std::result::Result<String, String>,
) -> std::result::Result<T, String> {
    let text = reply?;
    serde_json::from_str(&text).map_err(|error| error.to_string())
}

/// Await a reply; a closed channel means the originating callback has ended.
async fn settle(reply: wasmtime::Result<Reply>) -> std::result::Result<String, String> {
    let reply = reply.map_err(|error| error.to_string())?;
    reply
        .await
        .map_err(|_| "the originating callback has ended".to_owned())?
}

fn text(snapshot: Snapshot) -> SnapshotText {
    SnapshotText {
        value: snapshot.value.map(|value| value.to_string()),
        revision: snapshot.revision,
        version: snapshot.version,
    }
}

/// Parse kernel JSON state text from the guest.
fn json_text(value: &str) -> std::result::Result<Value, String> {
    serde_json::from_str(value).map_err(|error| format!("state is not JSON: {error}"))
}

/// Refuse any host operation while a selection waits for its call.
fn unselected(state: &mut State) -> wasmtime::Result<()> {
    if state.selection.take().is_some() {
        return Err(wasmtime::Error::msg(
            "a selection must be followed by its call in the same turn",
        ));
    }
    Ok(())
}

/// Issue one synchronous host request for the calling task group.
fn ask(state: &mut State, packet: Value) -> wasmtime::Result<Reply> {
    unselected(state)?;
    host_request(state, packet, false)
}

/// Link the typed `tembra:orb/host` functions.
pub(crate) fn link_host(linker: &mut Linker<State>) -> wasmtime::Result<()> {
    let mut host = linker.instance(tembra_contracts::HOST)?;
    host.func_wrap("environment", |store, (): ()| {
        Ok((super::worker::current_environment(store.data())?,))
    })?;
    // Diagnostics need no callback authority: the host attributes them to this revision's Orb
    // and delivers them to the engine's log sink without a scope request.
    host.func_wrap("log", |store, (level, message): (Level, String)| {
        use crate::protocol::LogLevel;
        let level = match level {
            Level::Log => LogLevel::Log,
            Level::Debug => LogLevel::Debug,
            Level::Info => LogLevel::Info,
            Level::Warn => LogLevel::Warn,
            Level::Error => LogLevel::Error,
        };
        if let Some(diagnostics) = &store.data().diagnostics {
            diagnostics.emit(level, message);
        }
        Ok(())
    })?;
    macro_rules! sync {
        ($name:literal, $params:ty, |$args:pat_param| $packet:expr, $reply:ty, |$value:ident| $map:expr) => {
            host.func_wrap_async($name, |mut store, $args: $params| {
                let reply = ask(store.data_mut(), $packet);
                Box::new(async move {
                    let reply = decode::<$reply>(settle(reply).await);
                    Ok((reply.map(|$value| $map),))
                })
            })?;
        };
    }
    sync!(
        "instance-address",
        (),
        |()| json!({"operation": "address"}),
        Address,
        |address| address
    );
    sync!(
        "root",
        (),
        |()| json!({"operation": "orb"}),
        Option<u64>,
        |root| root
    );
    sync!(
        "context-check",
        (),
        |()| json!({"operation": "context.check"}),
        Value,
        |_unit| ()
    );
    sync!(
        "context-principal",
        (),
        |()| json!({"operation": "context.principal"}),
        String,
        |principal| principal
    );
    sync!(
        "context-cancel",
        (),
        |()| json!({"operation": "context.cancel"}),
        Value,
        |_unit| ()
    );
    sync!(
        "signal-cancelled",
        (),
        |()| json!({"operation": "signal.cancelled"}),
        bool,
        |cancelled| cancelled
    );
    host.func_wrap_async(
        "state-declare",
        |mut store, (initial, version, validate): (String, u32, Option<u32>)| {
            let reply = json_text(&initial).map(|initial| {
                ask(
                    store.data_mut(),
                    json!({"operation": "state.declare", "initial": initial, "version": version, "validate": validate}),
                )
            });
            Box::new(async move {
                let reply = match reply {
                    Ok(reply) => decode::<u32>(settle(reply).await),
                    Err(error) => Err(error),
                };
                Ok((reply,))
            })
        },
    )?;
    sync!(
        "state-read",
        (Handle, Option<String>),
        |(state, known)| json!({"operation": "state.read", "scope": state.scope, "id": state.id, "known": known}),
        Snapshot,
        |snapshot| text(snapshot)
    );
    host.func_wrap_async(
        "state-update",
        |mut store, (state, value): (Handle, String)| {
            let reply = json_text(&value).map(|value| {
                ask(
                    store.data_mut(),
                    json!({"operation": "state.update", "scope": state.scope, "id": state.id, "value": value}),
                )
            });
            Box::new(async move {
                let reply = match reply {
                    Ok(reply) => decode::<Snapshot>(settle(reply).await).map(text),
                    Err(error) => Err(error),
                };
                Ok((reply,))
            })
        },
    )?;
    sync!(
        "hook-declare",
        (String,),
        |(alias,)| json!({"operation": "hook.declare", "name": alias}),
        u32,
        |id| id
    );
    sync!(
        "reference-declare",
        (String, Vec<String>),
        |(binding, key)| json!({"operation": "reference.declare", "service": binding, "key": key}),
        u32,
        |id| id
    );
    sync!(
        "reference-release",
        (Handle,),
        |(reference,)| json!({"operation": "reference.release", "scope": reference.scope, "id": reference.id}),
        Value,
        |_unit| ()
    );
    sync!(
        "reference-status",
        (Handle,),
        |(reference,)| json!({"operation": "reference.status", "scope": reference.scope, "id": reference.id}),
        String,
        |status| status
    );
    sync!(
        "reference-state",
        (Handle, Option<String>),
        |(reference, known)| json!({"operation": "reference.state", "scope": reference.scope, "id": reference.id, "known": known}),
        Snapshot,
        |snapshot| text(snapshot)
    );
    sync!(
        "resource-declare",
        (String, u32, u32, u32, Vec<Handle>),
        |(name, version, open, close, dependencies)| json!({
            "operation": "resource.declare",
            "name": name,
            "version": version,
            "open": open,
            "close": close,
            "dependencies": dependencies
                .iter()
                .map(|handle| json!({"scope": handle.scope, "id": handle.id}))
                .collect::<Vec<_>>(),
        }),
        u32,
        |id| id
    );
    sync!(
        "resource-read",
        (Handle,),
        |(resource,)| json!({"operation": "resource.read", "scope": resource.scope, "id": resource.id}),
        u32,
        |value| value
    );
    macro_rules! asynchronous {
        ($name:literal, $params:ty, |$args:pat_param| $packet:expr, $reply:ty, |$value:ident| $map:expr) => {
            host.func_wrap_concurrent($name, |accessor: &Accessor<State>, $args: $params| {
                let reply = accessor.with(|mut access| {
                    let state = access.data_mut();
                    unselected(state)?;
                    host_request(state, $packet, true)
                });
                Box::pin(async move {
                    let reply = decode::<$reply>(settle(reply).await);
                    Ok((reply.map(|$value| $map),))
                })
            })?;
        };
    }
    asynchronous!(
        "reference-ready",
        (Handle,),
        |(reference,)| json!({"operation": "reference.ready", "scope": reference.scope, "id": reference.id}),
        Value,
        |_unit| ()
    );
    asynchronous!(
        "reference-acquire",
        (String, Vec<String>),
        |(binding, key)| json!({"operation": "reference.acquire", "service": binding, "key": key}),
        u32,
        |id| id
    );
    asynchronous!(
        "wait",
        (u32,),
        |(milliseconds,)| json!({"operation": "wait", "milliseconds": milliseconds}),
        Value,
        |_unit| ()
    );
    host.func_wrap_async("select", |mut store, (reference,): (Option<Handle>,)| {
        // A reference the caller may not use (unknown, foreign table, released root claim or
        // another root) is refused here, where the guest can catch it, instead of trapping the
        // typed import that follows.
        let checked = reference.map(|handle| {
            ask(
                store.data_mut(),
                json!({"operation": "reference.status", "scope": handle.scope, "id": handle.id}),
            )
        });
        Box::new(async move {
            if let Some(checked) = checked {
                // A released or gone reference cannot serve the call that would follow.
                let usable =
                    decode::<String>(settle(checked).await).and_then(|status| {
                        match status.as_str() {
                            "released" => Err("the reference was released".to_owned()),
                            "gone" => Err("the reference is no longer available".to_owned()),
                            _ => Ok(()),
                        }
                    });
                if let Err(error) = usable {
                    // A refused selection leaves no pending selection behind.
                    store.data_mut().selection = None;
                    return Ok((Err(error),));
                }
            }
            select(store.data_mut(), reference.map(Selection::Reference))
        })
    })?;
    host.func_wrap("select-hook", |mut store, (hook,): (Option<Handle>,)| {
        select(store.data_mut(), hook.map(Selection::Hook))
    })?;
    Ok(())
}

/// Record a selection for the next call of the same turn; only an admitted callback may select.
fn select(
    state: &mut State,
    selection: Option<Selection>,
) -> wasmtime::Result<(std::result::Result<(), String>,)> {
    if super::worker::current_environment(state).is_err() {
        return Ok((Err("selection requires an admitted callback".into()),));
    }
    let Some(selection) = selection else {
        state.selection = None;
        return Ok((Ok(()),));
    };
    if state.selection.replace(selection).is_some() {
        state.selection = None;
        return Ok((Err("a previous selection was never called".into()),));
    }
    Ok((Ok(()),))
}

/// Link each self-import of an exported hook interface as a run of the selected declared hook.
///
/// Only interfaces the contract exports and does not import are self-imports; the verifier has
/// already checked that their signatures equal the exports'.
pub(crate) fn link_hooks(
    linker: &mut Linker<State>,
    engine: &wasmtime::Engine,
    component: &wasmtime::component::Component,
    contract: &tembra_contracts::PreparedContract,
) -> wasmtime::Result<()> {
    let exported = |name: &str| {
        contract
            .model()
            .exports
            .iter()
            .any(|entry| entry.key == name)
    };
    let imported = |name: &str| {
        contract
            .model()
            .imports
            .iter()
            .any(|entry| entry.key == name)
    };
    for (name, item) in component.component_type().imports(engine) {
        if !exported(name) || imported(name) {
            continue;
        }
        let ComponentItem::ComponentInstance(instance) = item.ty else {
            continue;
        };
        let mut linked = linker.instance(name)?;
        for (function, item) in instance.exports(engine) {
            let ComponentItem::ComponentFunc(ty) = item.ty else {
                continue;
            };
            // A synchronous WIT hook function blocks its caller; an `async func` runs
            // concurrently. Both run the selected declared hook.
            if ty.async_() {
                linked.func_new_concurrent(function, move |accessor, ty, params, results| {
                    let reply = accessor.with(|mut access| run_hook(access.data_mut(), params));
                    Box::pin(async move { finish_hook(reply, &ty, results).await })
                })?;
            } else {
                linked.func_new_async(function, move |mut store, ty, params, results| {
                    let reply = run_hook(store.data_mut(), params);
                    Box::new(async move { finish_hook(reply, &ty, results).await })
                })?;
            }
        }
    }
    Ok(())
}

/// Issue the selected hook's run with the call's single typed input.
fn run_hook(state: &mut State, params: &[Val]) -> wasmtime::Result<Reply> {
    let hook = match state.selection.take() {
        Some(Selection::Hook(hook)) => hook,
        _ => {
            return Err(wasmtime::Error::msg(
                "a hook interface call requires a selected declared hook",
            ));
        }
    };
    let [value] = params else {
        return Err(wasmtime::Error::msg("a hook callback takes one input"));
    };
    let value = super::values::lift(value.clone())
        .map_err(|error| wasmtime::Error::msg(error.to_string()))?;
    host_request(
        state,
        json!({"operation": "hook.run", "scope": hook.scope, "id": hook.id, "value": value}),
        true,
    )
}

/// Lower the hook result with the import's actual type.
async fn finish_hook(
    reply: wasmtime::Result<Reply>,
    ty: &wasmtime::component::types::ComponentFunc,
    results: &mut [Val],
) -> wasmtime::Result<()> {
    let value = settle(reply).await.map_err(wasmtime::Error::msg)?;
    let value: Value = serde_json::from_str(&value).map_err(wasmtime::Error::msg)?;
    let value = super::worker::lower_result(ty, &value)?;
    super::worker::store_result(results, value)
}

/// Link the runtime's WASI 0.2 internals to the Store's fixed internal context.
pub(crate) fn link_internals(linker: &mut Linker<State>) -> wasmtime::Result<()> {
    use wasmtime_wasi::{
        cli::{WasiCli, WasiCliCtxView},
        clocks::{WasiClocks, WasiClocksCtxView},
        filesystem::{WasiFilesystem, WasiFilesystemCtxView},
        p2::bindings::{cli, clocks, filesystem, io, random},
        random::WasiRandom,
    };
    // Semver-compatible linking serves every 0.2.x patch release the runtime imports.
    let l = linker;
    io::error::add_to_linker::<State, Table>(l, |state| &mut state.table)?;
    io::poll::add_to_linker::<State, Table>(l, |state| &mut state.table)?;
    io::streams::add_to_linker::<State, Table>(l, |state| &mut state.table)?;
    fn clocks(state: &mut State) -> WasiClocksCtxView<'_> {
        WasiClocksCtxView {
            ctx: state.internal.clocks(),
            table: &mut state.table,
        }
    }
    clocks::wall_clock::add_to_linker::<State, WasiClocks>(l, clocks)?;
    clocks::monotonic_clock::add_to_linker::<State, WasiClocks>(l, clocks)?;
    random::random::add_to_linker::<State, WasiRandom>(l, |state| state.internal.random())?;
    fn files(state: &mut State) -> WasiFilesystemCtxView<'_> {
        WasiFilesystemCtxView {
            ctx: state.internal.filesystem(),
            table: &mut state.table,
        }
    }
    filesystem::types::add_to_linker::<State, WasiFilesystem>(l, files)?;
    filesystem::preopens::add_to_linker::<State, WasiFilesystem>(l, files)?;
    fn cli(state: &mut State) -> WasiCliCtxView<'_> {
        WasiCliCtxView {
            ctx: state.internal.cli(),
            table: &mut state.table,
        }
    }
    cli::exit::add_to_linker::<State, WasiCli>(l, cli)?;
    cli::environment::add_to_linker::<State, WasiCli>(l, cli)?;
    cli::stdin::add_to_linker::<State, WasiCli>(l, cli)?;
    cli::stdout::add_to_linker::<State, WasiCli>(l, cli)?;
    cli::stderr::add_to_linker::<State, WasiCli>(l, cli)?;
    cli::terminal_input::add_to_linker::<State, WasiCli>(l, cli)?;
    cli::terminal_output::add_to_linker::<State, WasiCli>(l, cli)?;
    cli::terminal_stdin::add_to_linker::<State, WasiCli>(l, cli)?;
    cli::terminal_stdout::add_to_linker::<State, WasiCli>(l, cli)?;
    cli::terminal_stderr::add_to_linker::<State, WasiCli>(l, cli)?;
    Ok(())
}

/// Resource-table access for the WASI I/O interfaces.
struct Table;

impl wasmtime::component::HasData for Table {
    type Data<'a> = &'a mut wasmtime::component::ResourceTable;
}

/// The runtime's fixed internal context: no environment, arguments, preopens or network, empty
/// stdin and discarded stdout and stderr (console output goes through `host.log`). Clocks are
/// the installation's configured clocks only when it already holds the clocks grant; otherwise
/// they are frozen at zero. Randomness is a fixed seed and is never security material.
pub(crate) fn internal_context(clocks: Option<crate::providers::Clocks>) -> wasmtime_wasi::WasiCtx {
    let mut builder = wasmtime_wasi::WasiCtxBuilder::new();
    builder
        .secure_random(wasmtime_wasi::Deterministic::new(vec![0]))
        .insecure_random(wasmtime_wasi::Deterministic::new(vec![0]))
        .insecure_random_seed(0)
        .allow_tcp(false)
        .allow_udp(false)
        .allow_ip_name_lookup(false);
    match clocks {
        Some(clocks) => {
            builder
                .wall_clock(Wall(clocks.wall))
                .monotonic_clock(Monotonic(clocks.monotonic));
        }
        None => {
            builder.wall_clock(Frozen).monotonic_clock(Frozen);
        }
    }
    builder.build()
}

struct Frozen;

impl wasmtime_wasi::HostWallClock for Frozen {
    fn resolution(&self) -> std::time::Duration {
        std::time::Duration::from_nanos(1)
    }

    fn now(&self) -> std::time::Duration {
        std::time::Duration::ZERO
    }
}

impl wasmtime_wasi::HostMonotonicClock for Frozen {
    fn resolution(&self) -> u64 {
        1
    }

    fn now(&self) -> u64 {
        0
    }
}

struct Wall(Box<dyn wasmtime_wasi::HostWallClock>);

impl wasmtime_wasi::HostWallClock for Wall {
    fn resolution(&self) -> std::time::Duration {
        self.0.resolution()
    }

    fn now(&self) -> std::time::Duration {
        self.0.now()
    }
}

struct Monotonic(Box<dyn wasmtime_wasi::HostMonotonicClock>);

impl wasmtime_wasi::HostMonotonicClock for Monotonic {
    fn resolution(&self) -> u64 {
        self.0.resolution()
    }

    fn now(&self) -> u64 {
        self.0.now()
    }
}

/// The lifecycle exports of one prepared source Orb instance.
pub(crate) struct Lifecycle {
    pub describe: Func,
    pub create: Func,
    pub release: Func,
    pub validate: Func,
    pub open: Func,
    pub close: Func,
}

impl Lifecycle {
    /// Resolve every lifecycle export; the verifier already checked their types.
    pub(crate) fn resolve(
        store: &mut wasmtime::Store<State>,
        instance: &Instance,
    ) -> wasmtime::Result<Self> {
        let (_, interface) = instance
            .get_export(&mut *store, None, tembra_contracts::LIFECYCLE)
            .ok_or_else(|| wasmtime::Error::msg("missing source lifecycle"))?;
        let mut function = |name: &str| {
            let (_, index) = instance
                .get_export(&mut *store, Some(&interface), name)
                .ok_or_else(|| wasmtime::Error::msg(format!("missing lifecycle {name}")))?;
            instance
                .get_func(&mut *store, index)
                .ok_or_else(|| wasmtime::Error::msg(format!("lifecycle {name} is not a function")))
        };
        Ok(Self {
            describe: function("describe")?,
            create: function("create")?,
            release: function("release")?,
            validate: function("validate")?,
            open: function("open")?,
            close: function("close")?,
        })
    }
}

/// Convert `lifecycle.describe`'s typed record into the checked implementation description.
pub(crate) fn description(value: &Val) -> wasmtime::Result<Value> {
    let invalid = || wasmtime::Error::msg("lifecycle description has an unexpected shape");
    let Val::Record(fields) = value else {
        return Err(invalid());
    };
    let field = |name: &str| {
        fields
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, value)| value)
            .ok_or_else(invalid)
    };
    let string = |value: &Val| match value {
        Val::String(text) => Ok(text.clone()),
        _ => Err(invalid()),
    };
    let list = |value: &Val| match value {
        Val::List(items) => Ok(items.clone()),
        _ => Err(invalid()),
    };
    let record = |value: &Val| match value {
        Val::Record(fields) => Ok(fields.clone()),
        _ => Err(invalid()),
    };
    let enumeration = |value: &Val| match value {
        Val::Enum(case) => Ok(case.clone()),
        _ => Err(invalid()),
    };
    let lookup = |fields: &[(String, Val)], name: &str| {
        fields
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, value)| value.clone())
            .ok_or_else(invalid)
    };
    let mut services = serde_json::Map::new();
    for service in list(field("services")?)? {
        let service = record(&service)?;
        let methods = list(&lookup(&service, "methods")?)?
            .iter()
            .map(string)
            .collect::<wasmtime::Result<Vec<_>>>()?;
        let lifetime = match lookup(&service, "lifetime")? {
            Val::Option(Some(lifetime)) => Some(enumeration(&lifetime)?),
            Val::Option(None) => None,
            _ => return Err(invalid()),
        };
        let linger = match lookup(&service, "linger")? {
            Val::Option(Some(linger)) => match *linger {
                Val::U32(linger) => Some(linger),
                _ => return Err(invalid()),
            },
            Val::Option(None) => None,
            _ => return Err(invalid()),
        };
        let access = match lookup(&service, "access")? {
            Val::Bool(access) => access,
            _ => return Err(invalid()),
        };
        let mut entry = serde_json::Map::new();
        entry.insert("methods".into(), json!(methods));
        if let Some(lifetime) = lifetime {
            entry.insert("lifetime".into(), json!(lifetime));
        }
        if let Some(linger) = linger {
            entry.insert("linger".into(), json!(linger));
        }
        entry.insert("access".into(), json!(access));
        let alias = string(&lookup(&service, "alias")?)?;
        if services.insert(alias, Value::Object(entry)).is_some() {
            return Err(wasmtime::Error::msg("duplicate described service"));
        }
    }
    let mut hooks = serde_json::Map::new();
    for hook in list(field("hooks")?)? {
        let hook = record(&hook)?;
        let alias = string(&lookup(&hook, "alias")?)?;
        let composition = enumeration(&lookup(&hook, "composition")?)?;
        if hooks.insert(alias, json!(composition)).is_some() {
            return Err(wasmtime::Error::msg("duplicate described hook"));
        }
    }
    let mut contributes = serde_json::Map::new();
    for contribution in list(field("contributes")?)? {
        let contribution = record(&contribution)?;
        let binding = string(&lookup(&contribution, "binding")?)?;
        let Val::S32(priority) = lookup(&contribution, "priority")? else {
            return Err(invalid());
        };
        if contributes.insert(binding, json!(priority)).is_some() {
            return Err(wasmtime::Error::msg("duplicate described contribution"));
        }
    }
    let Val::U32(api_version) = field("api-version")? else {
        return Err(invalid());
    };
    Ok(json!({
        "name": string(field("name")?)?,
        "apiVersion": api_version,
        "services": services,
        "hooks": hooks,
        "contributes": contributes,
    }))
}

/// The guest's `result<_, string>` or `result<u32, string>` as a host reply.
pub(crate) fn outcome(value: Option<Val>) -> std::result::Result<Value, String> {
    match value {
        Some(Val::Result(Ok(None))) | None => Ok(Value::Null),
        Some(Val::Result(Ok(Some(value)))) => match *value {
            Val::U32(value) => Ok(json!(value)),
            _ => Err("unexpected lifecycle result".into()),
        },
        Some(Val::Result(Err(Some(error)))) => match *error {
            Val::String(error) => Err(error),
            _ => Err("unexpected lifecycle error".into()),
        },
        _ => Err("unexpected lifecycle result".into()),
    }
}

/// Shared ownership of lifecycle functions across commands.
pub(crate) type Shared = Arc<Lifecycle>;
