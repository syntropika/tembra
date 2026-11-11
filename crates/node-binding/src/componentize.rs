//! Prepare a closed ES-module source Orb as a verified component artifact.
//!
//! The prebuilt QuickJS runtime dylib (dquickts with Tembra's componentize-qjs runtime patches)
//! is linked with wit-dylib bindings for the contract's private source world, the module is
//! evaluated once under Wizer, and the snapshot is checked and packaged. Engine bytes are an
//! input: editing JavaScript never rebuilds the runtime.
//!
//! Build policy:
//! - The snapshot runs with deterministic runtime internals (clocks frozen at zero, seeded
//!   random, no arguments, environment, preopens or network, in-memory stdio). Every other
//!   import, including the public contract's and the private `tembra:orb/host`, traps, so
//!   evaluating a module never calls the host.
//! - Afterwards only the runtime's build-time module loader is replaced by a trapping stub.
//!   The WASI 0.2 runtime internals (`tembra_contracts::runtime_internal`) stay imports: the
//!   host links them from a fixed private context, so `Date.now()` and panic diagnostics work
//!   without user grants. Imports the source world declares are never stubbed, and any other
//!   import fails the build.
//! - The public contract then verifies the actual component and packages it.

mod link;
mod shim;

#[cfg(test)]
mod edge_tests;
#[cfg(test)]
mod rejection_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod type_only_tests;

use std::collections::BTreeSet;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use tembra_contracts::Contract;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::p2::pipe::{MemoryInputPipe, MemoryOutputPipe};
use wasmtime_wasi::{
    HostMonotonicClock, HostWallClock, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView,
};
use wasmtime_wizer::{WasmtimeWizerComponent, Wizer};
use wit_parser::Resolve;

/// The runtime's build-only module loader; closed sources never load modules.
const MODULE_LOADER: &str = "local:init/module-loader";

/// The one production authoring path: componentize against the private source world for
/// `hooks`, then verify and package against the public contract.
pub(crate) fn source_artifact(
    public: &Contract,
    source: &str,
    runtime: &[u8],
    name: &str,
    api_version: u32,
    hooks: &BTreeSet<String>,
) -> Result<Vec<u8>> {
    let world = public.source_world(hooks)?;
    let component = componentize_with(&world, source, runtime, &type_only_imports(public))?;
    // Verifies the actual component types against the public contract before packaging.
    Ok(public.source_component_artifact(&component, name, api_version)?)
}

/// Interfaces the public world imports only because an explicit import uses their types, and
/// which define or use no resource but do declare functions. Those functions grant authority the
/// contract never asked for, so the build replaces them with trapping stubs; record and enum
/// types stay structural in the importing interfaces. A function-less type interface carries no
/// authority and stays as it is. An implicit interface with resources (such as `wasi:http/types`
/// under `wasi:http/client`) is callable by construction and stays a real import.
pub(crate) fn type_only_imports(public: &Contract) -> BTreeSet<String> {
    public
        .prepared()
        .model()
        .imports
        .iter()
        .filter_map(|entry| match &entry.item {
            tembra_contracts::Item::Interface(interface)
                if !interface.explicit
                    && interface.resources.is_empty()
                    && !interface.functions.is_empty() =>
            {
                Some(entry.key.clone())
            }
            _ => None,
        })
        .collect()
}

/// Build a component for `world` from one closed ES module. Public contract policy is the
/// caller's: this applies only the import policy above and structural validation.
#[cfg(test)]
pub(crate) fn componentize(world: &Contract, source: &str, runtime: &[u8]) -> Result<Vec<u8>> {
    componentize_with(world, source, runtime, &BTreeSet::new())
}

/// Build a component and remove the `type_only` imports the caller derived from the public
/// contract with [`type_only_imports`].
pub(crate) fn componentize_with(
    world: &Contract,
    source: &str,
    runtime: &[u8],
    type_only: &BTreeSet<String>,
) -> Result<Vec<u8>> {
    let wit = world.encode()?;
    let wit_parser::decoding::DecodedWasm::WitPackage(resolve, package) =
        wit_parser::decoding::decode(&wit).context("failed to decode the source world WIT")?
    else {
        bail!("the source world did not encode a WIT package");
    };
    let selected = resolve.select_world(&[package], Some(world.prepared().world()))?;
    let declared = import_names(&resolve, selected);
    let policy = Policy {
        declared: &declared,
        type_only,
    };

    let (mut bindings, metadata) = wit_dylib::create_with_metadata(&resolve, selected, None)
        .context("failed to generate WIT bindings")?;
    let shim = shim::generate(&resolve, &metadata);
    wit_component::embed_component_metadata(
        &mut bindings,
        &resolve,
        selected,
        wit_component::StringEncoding::UTF8,
        false,
    )?;
    let mut linker = wit_component::Linker::default();
    linker.encoder().validate(true);
    linker.library("componentize_qjs_runtime.wasm", runtime, false)?;
    linker.library("wit-dylib.wasm", &bindings, false)?;
    linker.encoder().adapter(
        "wasi_snapshot_preview1",
        wasi_preview1_component_adapter_provider::WASI_SNAPSHOT_PREVIEW1_REACTOR_ADAPTER,
    )?;
    let linked = linker.encode().context("failed to link the runtime")?;

    // A dedicated thread keeps the build independent of any runtime the caller has entered.
    let snapshot = std::thread::scope(|scope| {
        scope
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_time()
                    .build()?
                    .block_on(snapshot(&linked, &shim, source))
            })
            .join()
            .map_err(|_| anyhow!("the source Orb build panicked"))?
    })?;

    let (actual, actual_world) = link::world(&snapshot)?;
    let mut stubbed = BTreeSet::new();
    for name in import_names(&actual, actual_world) {
        match policy.classify(&name) {
            Import::Declared | Import::RuntimeInternal => {}
            Import::BuildOnly | Import::TypeOnly => {
                stubbed.insert(name);
            }
            Import::Unexpected => {
                bail!("the runtime imports `{name}`, which the source world does not declare")
            }
        }
    }
    let component = link::stub_imports(&snapshot, &stubbed)?;
    verify_imports(&policy, &component)?;
    Ok(component)
}

/// How the build treats one import of the snapshotted runtime.
#[derive(Debug, PartialEq, Eq)]
enum Import {
    /// Declared by the source world; never stubbed.
    Declared,
    /// A WASI 0.2 runtime internal the host links from its fixed private context.
    RuntimeInternal,
    /// Used only while building; replaced by a trapping stub.
    BuildOnly,
    /// Declared only for its types (see [`type_only_imports`]); replaced by a trapping stub.
    TypeOnly,
    /// Anything else fails the build.
    Unexpected,
}

/// The import policy of one build.
struct Policy<'a> {
    declared: &'a BTreeSet<String>,
    type_only: &'a BTreeSet<String>,
}

impl Policy<'_> {
    fn classify(&self, name: &str) -> Import {
        if self.type_only.contains(name) {
            Import::TypeOnly
        } else {
            Import::classify(name, self.declared)
        }
    }
}

impl Import {
    fn classify(name: &str, declared: &BTreeSet<String>) -> Self {
        if declared.contains(name) {
            Self::Declared
        } else if tembra_contracts::runtime_internal(name) {
            Self::RuntimeInternal
        } else if name == MODULE_LOADER {
            Self::BuildOnly
        } else {
            Self::Unexpected
        }
    }
}

/// Validate the actual component and re-check its decoded imports, never embedded metadata.
fn verify_imports(policy: &Policy<'_>, component: &[u8]) -> Result<()> {
    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(component)
        .context("componentized Orb failed validation")?;
    let (resolve, world) = link::world(component)?;
    for name in import_names(&resolve, world) {
        if !matches!(
            policy.classify(&name),
            Import::Declared | Import::RuntimeInternal
        ) {
            bail!("componentized Orb still imports `{name}`");
        }
    }
    Ok(())
}

fn import_names(resolve: &Resolve, world: wit_parser::WorldId) -> BTreeSet<String> {
    resolve.worlds[world]
        .imports
        .keys()
        .map(|key| resolve.name_world_key(key))
        .collect()
}

struct Bootstrap {
    wasi: WasiCtx,
    table: ResourceTable,
}

impl WasiView for Bootstrap {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

/// A clock frozen at zero, so a snapshot never captures build time.
struct Frozen;

impl HostWallClock for Frozen {
    fn resolution(&self) -> Duration {
        Duration::from_nanos(1)
    }
    fn now(&self) -> Duration {
        Duration::ZERO
    }
}

impl HostMonotonicClock for Frozen {
    fn resolution(&self) -> u64 {
        1
    }
    fn now(&self) -> u64 {
        0
    }
}

/// Evaluate the shim and module once and snapshot the initialized runtime.
async fn snapshot(component: &[u8], shim: &str, source: &str) -> Result<Vec<u8>> {
    use rand::SeedableRng;

    let output = MemoryOutputPipe::new(16 * 1024);
    let wasi = WasiCtxBuilder::new()
        .stdin(MemoryInputPipe::new(Vec::<u8>::new()))
        .stdout(output.clone())
        .stderr(output.clone())
        .wall_clock(Frozen)
        .monotonic_clock(Frozen)
        .secure_random(rand::rngs::StdRng::seed_from_u64(0))
        .insecure_random(rand::rngs::StdRng::seed_from_u64(0))
        .insecure_random_seed(0)
        .allow_ip_name_lookup(false)
        .allow_tcp(false)
        .allow_udp(false)
        .build();

    let mut config = Config::new();
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);
    config.wasm_component_model_map(true);
    let engine = Engine::new(&config)?;
    let mut store = Store::new(
        &engine,
        Bootstrap {
            wasi,
            table: ResourceTable::new(),
        },
    );
    // Snapshotting copies whole memories through host calls; allow it for this build store only.
    store.set_hostcall_fuel(1024 << 20);

    let (prepared, init) = link::prepare_snapshot(component)?;
    let mut wizer = Wizer::new();
    wizer.init_func(&init);
    let (cx, instrumented) = wizer.instrument_component(&prepared)?;
    let instrumented = Component::new(&engine, &instrumented)?;

    let mut linker = Linker::new(&engine);
    linker.allow_shadowing(true);
    linker.define_unknown_imports_as_traps(&instrumented)?;
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    let mut loader = linker.instance(MODULE_LOADER)?;
    let refuse = "a closed source Orb cannot import modules".to_owned();
    let message = refuse.clone();
    loader.func_wrap("resolve", move |_, (_, _): (String, String)| {
        Ok((Err::<String, String>(message.clone()),))
    })?;
    loader.func_wrap("load", move |_, (_,): (String,)| {
        Ok((Err::<String, String>(refuse.clone()),))
    })?;

    let instance = linker.instantiate_async(&mut store, &instrumented).await?;
    let run = instance.get_typed_func::<(&str, &str, Option<&str>, bool), (Result<(), String>,)>(
        &mut store, &init,
    )?;
    let (result,) = run
        .call_async(&mut store, (shim, source, None, false))
        .await
        .map_err(anyhow::Error::from)
        .context("source Orb evaluation trapped")?;
    result.map_err(|error| {
        let output = String::from_utf8_lossy(&output.contents()).into_owned();
        anyhow!("source Orb evaluation failed: {error}{output}")
    })?;
    wizer
        .snapshot_component(
            &cx,
            &mut WasmtimeWizerComponent {
                store: &mut store,
                instance,
            },
        )
        .await
        .map_err(Into::into)
}
