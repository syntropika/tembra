//! Implicit, resource-free WASI interfaces that a public world imports only for their types
//! (for example `wasi:clocks/system-clock` under `wasi:filesystem/types`) carry no authority:
//! the production preparer replaces them with trapping stubs, keeps their types structural in
//! the importing interfaces, and never grants or inherits a clock. Explicit imports and
//! resource-bearing implicit interfaces stay real imports.
//!
//! Requires `TEMBRA_QJS_RUNTIME` (the packaged runtime dylib).

use std::collections::BTreeSet;
use std::time::Duration;

use wasmtime::component::{Component, Linker, ResourceTable, Val};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{HostWallClock, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use super::{link, source_artifact, type_only_imports};

/// Minimal private lifecycle describing one service, so native admission can check it; the
/// callbacks have no semantics here.
fn lifecycle(service: &str, methods: &[&str]) -> String {
    format!(
        r#"
export const lifecycle = {{
  describe() {{
    return {{ name: "fixture", apiVersion: 1, hooks: [], contributes: [],
      services: [{{ alias: "{service}", methods: {methods:?}, lifetime: undefined, linger: undefined, access: false }}] }};
  }},
  create(environment, service) {{}},
  release(environment) {{}},
  validate(callback, value) {{}},
  async open(callback) {{ return 0; }},
  async close(callback, value) {{}},
}};
"#
    )
}

const FILES_WIT: &str = r#"
package tembra-fixture:files;

interface files {
  read: async func() -> string;
  clock: func() -> string;
}

world orb {
  import wasi:filesystem/types@0.3.0;
  import wasi:filesystem/preopens@0.3.0;
  export files;
}
"#;

const FILES_JS: &str = r#"
import { getDirectories } from "wasi:filesystem/preopens@0.3.0";
import { now } from "wasi:clocks/system-clock@0.3.0";
export const files = {
  async read() {
    const [[directory]] = getDirectories();
    const file = await directory.openAt({}, "input", {}, { read: true });
    const [stream, done] = file.readViaStream(0n);
    let text = "";
    for await (const chunk of stream) text += String.fromCharCode(...chunk);
    await done.read();
    file[Symbol.dispose]();
    directory[Symbol.dispose]();
    return text;
  },
  clock() {
    return String(now().seconds);
  },
};
"#;

const CLOCK_WIT: &str = r#"
package tembra-fixture:clock;

interface probe {
  seconds: func() -> string;
}

world orb {
  import wasi:clocks/system-clock@0.3.0;
  export probe;
}
"#;

const CLOCK_JS: &str = r#"
import { now } from "wasi:clocks/system-clock@0.3.0";
export const probe = { seconds() { return String(now().seconds); } };
"#;

const HTTP_WIT: &str = r#"
package tembra-fixture:web;

interface web {
  fields: func() -> u32;
}

world orb {
  import wasi:http/client@0.3.0;
  export web;
}
"#;

const HTTP_JS: &str = r#"
import { Fields } from "wasi:http/types@0.3.0";
export const web = {
  fields() {
    const fields = new Fields();
    fields[Symbol.dispose]();
    return 1;
  },
};
"#;

fn runtime() -> Vec<u8> {
    let path = std::env::var_os("TEMBRA_QJS_RUNTIME")
        .expect("set TEMBRA_QJS_RUNTIME to the prebuilt patched runtime dylib");
    std::fs::read(path).expect("read TEMBRA_QJS_RUNTIME")
}

fn contract(wit: &str) -> tembra_contracts::Contract {
    tembra_contracts::Contract::from_source("orb.wit", wit, Some("orb")).unwrap()
}

fn build(wit: &str, js: &str, service: &str, methods: &[&str]) -> Vec<u8> {
    source_artifact(
        &contract(wit),
        &format!("{js}\n{}", lifecycle(service, methods)),
        &runtime(),
        "fixture",
        1,
        &BTreeSet::new(),
    )
    .unwrap()
}

fn imports(component: &[u8]) -> BTreeSet<String> {
    let (resolve, world) = link::world(component).unwrap();
    super::import_names(&resolve, world)
}

fn block<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

struct Host {
    wasi: WasiCtx,
    table: ResourceTable,
}

impl WasiView for Host {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

const HOST_SECONDS: u64 = 1_234_567;

/// A host wall clock that differs from both build time (frozen at zero) and real time.
struct Configured;

impl HostWallClock for Configured {
    fn resolution(&self) -> Duration {
        Duration::from_secs(1)
    }
    fn now(&self) -> Duration {
        Duration::from_secs(HOST_SECONDS)
    }
}

/// Run one export of `bytes` under standard WASI with a preopen and a configured wall clock; the
/// private host is linked with default values and is never called by these exports.
async fn call(
    bytes: &[u8],
    pulley: bool,
    interface: &str,
    name: &str,
    directory: &std::path::Path,
) -> wasmtime::Result<String> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);
    if pulley {
        config.target("pulley64").unwrap();
    }
    let engine = Engine::new(&config).unwrap();
    let component = Component::new(&engine, bytes).unwrap();
    let mut builder = WasiCtxBuilder::new();
    builder
        .preopened_dir(directory, "/data", wasmtime_wasi::FsPerms::ReadOnly)
        .unwrap();
    builder.wall_clock(Configured);
    let mut store = Store::new(
        &engine,
        Host {
            wasi: builder.build(),
            table: ResourceTable::new(),
        },
    );
    let mut linker = Linker::<Host>::new(&engine);
    // Cranelift and Pulley alike link the one pinned runtime module the thin artifact imports.
    super::link_runtime(&engine, &component, &mut linker).unwrap();
    linker.allow_shadowing(true);
    linker
        .define_unknown_imports_as_default_values(&mut store, &component)
        .unwrap();
    wasmtime_wasi::p2::add_to_linker_async(&mut linker).unwrap();
    wasmtime_wasi::p3::add_to_linker(&mut linker).unwrap();
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let parent = instance
        .get_export_index(&mut store, None, interface)
        .unwrap();
    let index = instance
        .get_export_index(&mut store, Some(&parent), name)
        .unwrap();
    let func = instance.get_func(&mut store, index).unwrap();
    let mut results = [Val::Bool(false)];
    func.call_async(&mut store, &[], &mut results).await?;
    match &results[0] {
        Val::String(value) => Ok(value.clone()),
        other => panic!("unexpected result {other:?}"),
    }
}

/// Native admission through the real executor engine on both backends.
fn admit(
    bytes: &[u8],
    grants: serde_json::Value,
    directory: &std::path::Path,
) -> Vec<Result<(), String>> {
    let install = || {
        serde_json::from_value::<tembra_executor_wasm::Install>(
            serde_json::json!({ "grants": grants }),
        )
        .unwrap()
    };
    [
        tembra_executor_wasm::Engine::new(Default::default()),
        tembra_executor_wasm::Engine::interpreted(Default::default()),
    ]
    .into_iter()
    .map(|engine| {
        let engine = engine.unwrap().filesystem("data", directory).unwrap();
        engine
            .load(bytes, install())
            .map(|_| ())
            .map_err(|error| error.to_string())
    })
    .collect()
}

#[test]
fn filesystem_world_drops_its_type_only_clock_imports() {
    let public = contract(FILES_WIT);
    assert_eq!(
        type_only_imports(&public),
        BTreeSet::from(["wasi:clocks/system-clock@0.3.0".to_owned()])
    );
    let artifact = build(FILES_WIT, FILES_JS, "files", &["read", "clock"]);
    let imports = imports(&artifact);
    assert!(
        imports.contains("wasi:filesystem/types@0.3.0"),
        "{imports:?}"
    );
    assert!(
        imports.contains("wasi:filesystem/preopens@0.3.0"),
        "{imports:?}"
    );
    // The callable clock is gone. `wasi:clocks/types` declares types only and grants nothing;
    // the runtime's own WASI 0.2 clocks are host-provided internals, never the Orb's grant.
    assert!(
        !imports.contains("wasi:clocks/system-clock@0.3.0"),
        "the type-only clock is still a callable import: {imports:?}"
    );
    public.check_source_component(&artifact).unwrap();

    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("input"), "files-only").unwrap();
    // Admitted with only its filesystem grant: no clock grant is implied or required.
    let filesystem = serde_json::json!({
        "filesystem": [{"provider": "data", "mount": "/data", "writable": false}]
    });
    assert_eq!(
        admit(&artifact, filesystem, directory.path()),
        [Ok(()), Ok(())]
    );

    block(async {
        for pulley in [false, true] {
            let interface = "tembra-fixture:files/files";
            assert_eq!(
                call(&artifact, pulley, interface, "read", directory.path())
                    .await
                    .unwrap(),
                "files-only",
                "pulley={pulley}"
            );
            // The ungranted clock is a trapping stub: neither the host's configured clock nor the
            // frozen build clock is reachable.
            let error = call(&artifact, pulley, interface, "clock", directory.path())
                .await
                .unwrap_err();
            assert!(
                format!("{error:?}").contains("unreachable"),
                "pulley={pulley}: {error:?}"
            );
        }
    });
}

#[test]
fn an_explicit_clock_import_is_kept_and_requires_its_grant() {
    let public = contract(CLOCK_WIT);
    assert!(!type_only_imports(&public).contains("wasi:clocks/system-clock@0.3.0"));
    let artifact = build(CLOCK_WIT, CLOCK_JS, "probe", &["seconds"]);
    assert!(imports(&artifact).contains("wasi:clocks/system-clock@0.3.0"));
    let directory = tempfile::tempdir().unwrap();
    for refused in admit(&artifact, serde_json::json!({}), directory.path()) {
        let refused = refused.unwrap_err();
        assert!(refused.contains("without an Orb grant"), "{refused}");
    }
    assert_eq!(
        admit(
            &artifact,
            serde_json::json!({"system": ["clocks"]}),
            directory.path()
        ),
        [Ok(()), Ok(())]
    );
    block(async {
        for pulley in [false, true] {
            let seconds = call(
                &artifact,
                pulley,
                "tembra-fixture:clock/probe",
                "seconds",
                directory.path(),
            )
            .await
            .unwrap();
            assert_eq!(seconds, HOST_SECONDS.to_string(), "pulley={pulley}");
        }
    });
}

#[test]
fn http_client_keeps_its_resource_bearing_types_interface() {
    let public = contract(HTTP_WIT);
    let type_only = type_only_imports(&public);
    assert!(
        !type_only.contains("wasi:http/types@0.3.0"),
        "{type_only:?}"
    );
    assert!(
        type_only.iter().all(|name| !name.starts_with("wasi:http/")),
        "{type_only:?}"
    );
    let artifact = build(HTTP_WIT, HTTP_JS, "web", &["fields"]);
    let (resolve, world) = link::world(&artifact).unwrap();
    let types = resolve.worlds[world]
        .imports
        .iter()
        .find(|(key, _)| resolve.name_world_key(key) == "wasi:http/types@0.3.0")
        .map(|(_, item)| item.clone())
        .expect("wasi:http/types stays a real import");
    let wit_parser::WorldItem::Interface { id, .. } = types else {
        panic!("wasi:http/types is not an interface import");
    };
    assert!(
        resolve.interfaces[id]
            .functions
            .contains_key("[constructor]fields"),
        "the Fields constructor must remain a real host function"
    );
    assert!(imports(&artifact).contains("wasi:http/client@0.3.0"));
}
