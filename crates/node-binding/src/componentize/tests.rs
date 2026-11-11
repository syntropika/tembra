//! Behavioural gates against the real prebuilt runtime dylib named by `TEMBRA_QJS_RUNTIME`.
//!
//! Every fixture is a public contract built through the production path: the private source
//! world (public world + `tembra:orb/source`), the generic bindings, public verification and
//! packaging. Lifecycle callbacks are a minimal stub; the executor qualifies their semantics.

use std::collections::BTreeSet;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use wasmtime::component::{
    Component, Instance, Linker, ResourceTable, Source, StreamConsumer, StreamReader, StreamResult,
    Val,
};
use wasmtime::{AsContextMut, Config, Engine, Store, StoreContextMut};
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;
use wasmtime_wasi::{HostWallClock, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use super::{Import, componentize, import_names, link, source_artifact};

/// Minimal private lifecycle: enough to qualify the augmented world, no semantics.
const LIFECYCLE_JS: &str = r#"
export const lifecycle = {
  describe() { return { name: "fixture", apiVersion: 1, services: [], hooks: [], contributes: [] }; },
  create(environment, service) {},
  release(environment) {},
  validate(callback, value) {},
  async open(callback) { return 0; },
  async close(callback, value) {},
};
"#;

const EXACT_WIT: &str = r#"
package tembra-fixture:exact;

interface types {
  variant kind { small(u32), big(u64), signed(s64), empty }
  record payload {
    id: u64,
    delta: s64,
    label: string,
    pair: option<tuple<u64, s64>>,
    values: list<u64>,
    kind: kind,
    bytes: list<u8>,
  }
}

interface host-math {
  use types.{payload};
  echo-u64: func(x: u64) -> u64;
  echo-s64: func(x: s64) -> s64;
  transform: func(p: payload) -> payload;
}

interface calc {
  use types.{payload};
  roundtrip-u64: func(x: u64) -> u64;
  roundtrip-s64: func(x: s64) -> s64;
  constants: func() -> tuple<u64, s64>;
  transform: func(p: payload) -> payload;
  overflow-u64: func() -> u64;
  number-for-u64: func() -> u64;
  overflow-argument: func() -> string;
  calls: func() -> u32;
  now: func() -> string;
  marker: func() -> string;
}

world orb {
  import host-math;
  export calc;
}
"#;

const EXACT_JS: &str = r#"
import hostMath from "tembra-fixture:exact/host-math";

let calls = 0;
const big = (name, value) => {
  if (typeof value !== "bigint") throw new Error(`${name}: ${typeof value}`);
  return value;
};

export const calc = {
  roundtripU64(x) { calls++; return big("u64", hostMath.echoU64(big("arg", x))); },
  roundtripS64(x) { calls++; return big("s64", hostMath.echoS64(big("arg", x))); },
  constants() { calls++; return [18446744073709551615n, -9223372036854775808n]; },
  transform(p) {
    calls++;
    const r = hostMath.transform(p);
    return { ...r, values: [...r.values, r.id, BigInt.asUintN(64, r.delta)] };
  },
  overflowU64() { return 2n ** 64n; },
  numberForU64() { return 9007199254740993; },
  overflowArgument() {
    try { hostMath.echoU64(2n ** 64n); return "accepted"; }
    catch (error) { return error.name; }
  },
  calls() { return calls; },
  now() { return String(Date.now()); },
  marker() { return "MARKER"; },
};
"#;

const STREAM_WIT: &str = r#"
package tembra-fixture:frames;

interface frames {
  frames: async func(n: u32) -> stream<u8>;
  frames-fail: async func(n: u32, fail-at: u32) -> stream<u8>;
}

world orb {
  export frames;
}
"#;

const STREAM_JS: &str = r#"
const pattern = (i) => (i * 31 + 7) & 0xff;
async function* produce(n, failAt) {
  for (let offset = 0; offset < n; offset += 4096) {
    if (failAt !== undefined && offset >= failAt) throw new Error("producer failed");
    const chunk = new Uint8Array(Math.min(4096, n - offset));
    for (let j = 0; j < chunk.length; j++) chunk[j] = pattern(offset + j);
    yield chunk;
  }
}
export const frames = {
  async frames(n) { return produce(n); },
  async framesFail(n, failAt) { return produce(n, failAt); },
};
"#;

/// A public world importing WASI 0.3 filesystem, whose signatures carry futures.
const FILES_WIT: &str = r#"
package tembra-fixture:files;

interface probe {
  helpers: func() -> string;
}

world orb {
  import wasi:filesystem/types@0.3.0;
  export probe;
}
"#;

const FILES_JS: &str = r#"
import types from "wasi:filesystem/types@0.3.0";
export const probe = {
  helpers() { return `${typeof wit.Future.from}:${typeof wit.Stream.from}:${typeof types}`; },
};
"#;

const HOOK_WIT: &str = r#"
package tembra-fixture:hooks;

interface guard {
  check: func(value: u32) -> bool;
}

world orb {
  export guard;
}
"#;

const HOOK_JS: &str = r#"
import selfGuard from "tembra-fixture:hooks/guard";
export const guard = { check(value) { return value > 1; } };
"#;

fn runtime() -> &'static [u8] {
    static RUNTIME: OnceLock<Vec<u8>> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        let path = std::env::var_os("TEMBRA_QJS_RUNTIME").expect(
            "set TEMBRA_QJS_RUNTIME to the prebuilt patched dquickts runtime dylib \
             (runtime-async.wasm built from crates/executor-wasm/guest/patches/runtime.patch)",
        );
        std::fs::read(path).expect("read TEMBRA_QJS_RUNTIME")
    })
}

fn contract(wit: &str) -> tembra_contracts::Contract {
    tembra_contracts::Contract::from_source("orb.wit", wit, Some("orb")).unwrap()
}

/// Build through the single production path.
fn build_with(wit: &str, js: &str, hooks: &[&str]) -> anyhow::Result<Vec<u8>> {
    let hooks = hooks.iter().map(|alias| alias.to_string()).collect();
    source_artifact(
        &contract(wit),
        &format!("{js}\n{LIFECYCLE_JS}"),
        runtime(),
        "fixture",
        1,
        &hooks,
    )
}

fn build(wit: &str, js: &str) -> anyhow::Result<Vec<u8>> {
    build_with(wit, js, &[])
}

fn exact() -> &'static [u8] {
    static EXACT: OnceLock<Vec<u8>> = OnceLock::new();
    EXACT.get_or_init(|| build(EXACT_WIT, EXACT_JS).unwrap())
}

fn block<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
        .block_on(future)
}

fn engine() -> Engine {
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);
    Engine::new(&config).unwrap()
}

/// Stand-in for the executor's fixed private context for runtime internals: empty environment,
/// no preopens or network, captured stderr and a configured wall clock.
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

const CLOCK_SECONDS: u64 = 1_700_000_000;

struct Configured;

impl HostWallClock for Configured {
    fn resolution(&self) -> Duration {
        Duration::from_millis(1)
    }
    fn now(&self) -> Duration {
        Duration::from_secs(CLOCK_SECONDS)
    }
}

/// Link the runtime internals and `define`; every other import (the private host included)
/// traps if called.
async fn instantiate(
    bytes: &[u8],
    define: impl FnOnce(&mut Linker<Host>),
) -> (Store<Host>, Instance, MemoryOutputPipe) {
    instantiate_on(bytes, define, engine()).await
}

async fn instantiate_on(
    bytes: &[u8],
    define: impl FnOnce(&mut Linker<Host>),
    engine: Engine,
) -> (Store<Host>, Instance, MemoryOutputPipe) {
    let component = Component::new(&engine, bytes).unwrap();
    let mut linker = Linker::<Host>::new(&engine);
    linker.allow_shadowing(true);
    linker.define_unknown_imports_as_traps(&component).unwrap();
    wasmtime_wasi::p2::add_to_linker_async(&mut linker).unwrap();
    define(&mut linker);
    let stderr = MemoryOutputPipe::new(64 * 1024);
    let wasi = WasiCtxBuilder::new()
        .stderr(stderr.clone())
        .wall_clock(Configured)
        .allow_tcp(false)
        .allow_udp(false)
        .allow_ip_name_lookup(false)
        .build();
    let mut store = Store::new(
        &engine,
        Host {
            wasi,
            table: ResourceTable::new(),
        },
    );
    let instance = linker
        .instantiate_async(&mut store, &component)
        .await
        .unwrap();
    (store, instance, stderr)
}

async fn instantiate_exact(
    bytes: &[u8],
    echoes: Arc<AtomicU32>,
) -> (Store<Host>, Instance, MemoryOutputPipe) {
    instantiate(bytes, |linker| {
        let mut host = linker.instance("tembra-fixture:exact/host-math").unwrap();
        host.func_wrap("echo-u64", move |_, (x,): (u64,)| {
            echoes.fetch_add(1, Ordering::SeqCst);
            Ok((x,))
        })
        .unwrap();
        host.func_wrap("echo-s64", |_, (x,): (i64,)| Ok((x,)))
            .unwrap();
        host.func_new("transform", |_, _, params, results| {
            results[0] = params[0].clone();
            Ok(())
        })
        .unwrap();
    })
    .await
}

async fn call(
    store: &mut Store<Host>,
    instance: &Instance,
    interface: &str,
    name: &str,
    args: &[Val],
) -> wasmtime::Result<Val> {
    let interface = instance
        .get_export_index(&mut *store, None, interface)
        .unwrap();
    let index = instance
        .get_export_index(&mut *store, Some(&interface), name)
        .unwrap();
    let func = instance.get_func(&mut *store, index).unwrap();
    let mut results = [Val::Bool(false)];
    func.call_async(&mut *store, args, &mut results).await?;
    Ok(results[0].clone())
}

const CALC: &str = "tembra-fixture:exact/calc";

#[test]
fn exact_64_bit_values_cross_public_interfaces_of_the_source_world() {
    block(async {
        let (mut store, instance, _) = instantiate_exact(exact(), Arc::default()).await;
        let s = &mut store;
        assert_eq!(
            call(s, &instance, CALC, "roundtrip-u64", &[Val::U64(u64::MAX)])
                .await
                .unwrap(),
            Val::U64(u64::MAX)
        );
        assert_eq!(
            call(s, &instance, CALC, "roundtrip-s64", &[Val::S64(i64::MIN)])
                .await
                .unwrap(),
            Val::S64(i64::MIN)
        );
        assert_eq!(
            call(s, &instance, CALC, "constants", &[]).await.unwrap(),
            Val::Tuple(vec![Val::U64(u64::MAX), Val::S64(i64::MIN)])
        );
        let payload = Val::Record(vec![
            ("id".into(), Val::U64(u64::MAX)),
            ("delta".into(), Val::S64(-1)),
            ("label".into(), Val::String("x".into())),
            (
                "pair".into(),
                Val::Option(Some(Box::new(Val::Tuple(vec![
                    Val::U64(u64::MAX - 1),
                    Val::S64(i64::MIN),
                ])))),
            ),
            ("values".into(), Val::List(vec![Val::U64(1 << 63)])),
            (
                "kind".into(),
                Val::Variant("big".into(), Some(Box::new(Val::U64(u64::MAX)))),
            ),
            ("bytes".into(), Val::List(vec![Val::U8(7)])),
        ]);
        let Val::Record(fields) = call(s, &instance, CALC, "transform", &[payload])
            .await
            .unwrap()
        else {
            panic!("transform returned a non-record");
        };
        let field = |name: &str| {
            fields
                .iter()
                .find(|(field, _)| field == name)
                .unwrap()
                .1
                .clone()
        };
        assert_eq!(
            field("values"),
            Val::List(vec![
                Val::U64(1 << 63),
                Val::U64(u64::MAX),
                Val::U64(u64::MAX)
            ])
        );
        assert_eq!(
            field("kind"),
            Val::Variant("big".into(), Some(Box::new(Val::U64(u64::MAX))))
        );
        // The private lifecycle is exported next to the public interface.
        let described = call(s, &instance, tembra_contracts::LIFECYCLE, "describe", &[])
            .await
            .unwrap();
        assert!(matches!(described, Val::Record(_)), "{described:?}");
    });
}

#[test]
fn out_of_range_integers_are_refused_in_both_directions() {
    block(async {
        let echoes = Arc::new(AtomicU32::new(0));
        let (mut store, instance, _) = instantiate_exact(exact(), echoes.clone()).await;
        // Guest -> host arguments are recoverable JavaScript errors before the host is called.
        let result = call(&mut store, &instance, CALC, "overflow-argument", &[])
            .await
            .expect("an invalid import argument must not poison the component");
        assert_eq!(result, Val::String("RangeError".into()));
        assert_eq!(
            echoes.load(Ordering::SeqCst),
            0,
            "the host saw an out-of-range value"
        );
        let (mut store, instance, _) = instantiate_exact(exact(), echoes.clone()).await;
        // Export result: no error lane, so lowering traps instead of wrapping to 0.
        assert!(
            call(&mut store, &instance, CALC, "overflow-u64", &[])
                .await
                .is_err()
        );
        let (mut store, instance, _) = instantiate_exact(exact(), echoes).await;
        assert!(
            call(&mut store, &instance, CALC, "number-for-u64", &[])
                .await
                .is_err()
        );
    });
}

#[test]
fn only_declared_imports_and_runtime_internals_remain() {
    let (resolve, world) = link::world(exact()).unwrap();
    let imports = import_names(&resolve, world);
    assert!(
        imports.contains("tembra-fixture:exact/host-math"),
        "{imports:?}"
    );
    for name in &imports {
        assert!(
            name.starts_with("tembra-fixture:exact/")
                || name == tembra_contracts::HOST
                || tembra_contracts::runtime_internal(name),
            "unexpected import {name}"
        );
    }
    assert!(
        !imports.iter().any(|name| name.starts_with("local:")),
        "{imports:?}"
    );
}

#[test]
fn date_now_reads_the_hosts_internal_clock_without_a_grant() {
    block(async {
        let (mut store, instance, _) = instantiate_exact(exact(), Arc::default()).await;
        let now = call(&mut store, &instance, CALC, "now", &[]).await.unwrap();
        assert_eq!(now, Val::String((CLOCK_SECONDS * 1000).to_string()));
    });
}

#[test]
fn initialization_cannot_be_reinvoked() {
    let (resolve, world) = link::world(exact()).unwrap();
    let exports: BTreeSet<_> = resolve.worlds[world]
        .exports
        .keys()
        .map(|key| resolve.name_world_key(key))
        .collect();
    assert_eq!(
        exports,
        BTreeSet::from([CALC.to_owned(), tembra_contracts::LIFECYCLE.to_owned()])
    );
}

#[test]
fn building_never_runs_exports_or_calls_imports() {
    block(async {
        let (mut store, instance, _) = instantiate_exact(exact(), Arc::default()).await;
        assert_eq!(
            call(&mut store, &instance, CALC, "calls", &[])
                .await
                .unwrap(),
            Val::U32(0)
        );
    });
    for call in [
        "import math from \"tembra-fixture:exact/host-math\";\nmath.echoU64(1n);",
        "import host from \"tembra:orb/host@0.1.0\";\nhost.log(\"info\", \"evaluated\");",
        "Date.now() === 0 || (() => { throw new Error(\"build clock is not frozen\"); })();",
    ] {
        let result = build(EXACT_WIT, &format!("{EXACT_JS}\n{call}\n"));
        if call.starts_with("Date") {
            result.unwrap();
            continue;
        }
        let error = result.unwrap_err();
        assert!(format!("{error:#}").contains("trapped"), "{error:#}");
    }
}

#[test]
fn editing_javascript_reuses_the_engine_code() {
    let edited = build(EXACT_WIT, &EXACT_JS.replace("\"MARKER\"", "\"EDITED\"")).unwrap();
    assert_ne!(exact(), edited.as_slice());
    assert_eq!(code_sections(exact()), code_sections(&edited));
    block(async {
        let (mut store, instance, _) = instantiate_exact(exact(), Arc::default()).await;
        assert_eq!(
            call(&mut store, &instance, CALC, "marker", &[])
                .await
                .unwrap(),
            Val::String("MARKER".into())
        );
        let (mut store, instance, _) = instantiate_exact(&edited, Arc::default()).await;
        assert_eq!(
            call(&mut store, &instance, CALC, "marker", &[])
                .await
                .unwrap(),
            Val::String("EDITED".into())
        );
    });
}

/// Every core code section in the component, nested modules included.
fn code_sections(component: &[u8]) -> BTreeSet<Vec<u8>> {
    wasmparser::Parser::new(0)
        .parse_all(component)
        .filter_map(|payload| match payload.unwrap() {
            payload @ wasmparser::Payload::CodeSectionStart { .. } => payload
                .as_section()
                .map(|(_, range)| component[range.start as usize..range.end as usize].to_vec()),
            _ => None,
        })
        .collect()
}

#[test]
fn closed_sources_cannot_load_modules() {
    let error = build(EXACT_WIT, &format!("import './other.js';\n{EXACT_JS}")).unwrap_err();
    assert!(
        format!("{error:#}").contains("cannot import modules"),
        "{error:#}"
    );
}

#[test]
fn wasi_filesystem_futures_keep_generic_bindings() {
    let artifact = build(FILES_WIT, FILES_JS).unwrap();
    let (resolve, world) = link::world(&artifact).unwrap();
    assert!(import_names(&resolve, world).contains("wasi:filesystem/types@0.3.0"));
    block(async {
        let (mut store, instance, _) = instantiate(&artifact, |_| {}).await;
        let helpers = call(
            &mut store,
            &instance,
            "tembra-fixture:files/probe",
            "helpers",
            &[],
        )
        .await
        .unwrap();
        assert_eq!(helpers, Val::String("function:function:object".into()));
    });
}

#[test]
fn hook_self_imports_use_the_public_signatures() {
    let artifact = build_with(HOOK_WIT, HOOK_JS, &["guard"]).unwrap();
    let (resolve, world) = link::world(&artifact).unwrap();
    assert!(import_names(&resolve, world).contains("tembra-fixture:hooks/guard"));
    // Without the hook, the self-import is not part of the source world.
    assert!(build(HOOK_WIT, HOOK_JS).is_err());
}

#[test]
fn import_policy_never_masks_declared_imports() {
    let declared = BTreeSet::from(["wasi:clocks/wall-clock@0.3.0".to_owned()]);
    for (name, expected) in [
        ("wasi:clocks/wall-clock@0.3.0", Import::Declared),
        ("wasi:clocks/wall-clock@0.2.6", Import::RuntimeInternal),
        ("wasi:cli/stderr@0.2.6", Import::RuntimeInternal),
        ("local:init/module-loader", Import::BuildOnly),
        ("wasi:sockets/tcp@0.2.6", Import::Unexpected),
        ("wasi:http/types@0.2.6", Import::Unexpected),
        ("wasi:random/random@0.3.0", Import::Unexpected),
        ("tembra:orb/host@0.1.0", Import::Unexpected),
    ] {
        assert_eq!(Import::classify(name, &declared), expected, "{name}");
    }
}

#[test]
fn generic_builder_does_not_apply_public_profiles() {
    // A private world may carry futures; public profile policy belongs to the caller.
    let world = contract(
        "package tembra-fixture:later;\ninterface api { later: async func() -> future<u32>; }\nworld orb { export api; }\n",
    );
    let component = componentize(
        &world,
        "export const api = { async later() { return wit.Future.from(Promise.resolve(1), wit.Future.U32).readable; } };",
        runtime(),
    )
    .unwrap();
    let (resolve, decoded) = link::world(&component).unwrap();
    let exports: Vec<_> = resolve.worlds[decoded]
        .exports
        .keys()
        .map(|key| resolve.name_world_key(key))
        .collect();
    assert_eq!(exports, ["tembra-fixture:later/api"]);
    // The same world is refused at the public packaging boundary.
    assert!(world.prepared().check_resource_profile().is_err());
}

struct Collector {
    seen: Arc<Mutex<Vec<u8>>>,
    done: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for Collector {
    fn drop(&mut self) {
        if let Some(done) = self.done.take() {
            let _ = done.send(());
        }
    }
}

impl StreamConsumer<Host> for Collector {
    type Item = u8;

    fn poll_consume(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        mut store: StoreContextMut<'_, Host>,
        mut source: Source<'_, u8>,
        _: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        let mut values = Vec::with_capacity(source.remaining(&mut store)?);
        source.read(&mut store, &mut values)?;
        self.seen.lock().unwrap().extend(values);
        Poll::Ready(Ok(StreamResult::Completed))
    }
}

async fn read_frames(
    name: &str,
    args: (u32, Option<u32>),
) -> (wasmtime::Result<()>, Vec<u8>, String) {
    static FRAMES: OnceLock<Vec<u8>> = OnceLock::new();
    let bytes = FRAMES.get_or_init(|| build(STREAM_WIT, STREAM_JS).unwrap());
    let (mut store, instance, stderr) = instantiate(bytes, |_| {}).await;
    let interface = instance
        .get_export_index(&mut store, None, "tembra-fixture:frames/frames")
        .unwrap();
    let index = instance
        .get_export_index(&mut store, Some(&interface), name)
        .unwrap();
    let reader = match args {
        (n, None) => {
            instance
                .get_typed_func::<(u32,), (StreamReader<u8>,)>(&mut store, index)
                .unwrap()
                .call_async(&mut store, (n,))
                .await
                .unwrap()
                .0
        }
        (n, Some(fail)) => {
            instance
                .get_typed_func::<(u32, u32), (StreamReader<u8>,)>(&mut store, index)
                .unwrap()
                .call_async(&mut store, (n, fail))
                .await
                .unwrap()
                .0
        }
    };
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (done, finished) = tokio::sync::oneshot::channel();
    reader
        .pipe(
            &mut store,
            Collector {
                seen: seen.clone(),
                done: Some(done),
            },
        )
        .unwrap();
    let outcome = tokio::time::timeout(
        Duration::from_secs(60),
        store.as_context_mut().run_concurrent(async |_| {
            let _ = finished.await;
        }),
    )
    .await
    .expect("stream did not finish");
    let seen = seen.lock().unwrap().clone();
    (
        outcome,
        seen,
        String::from_utf8_lossy(&stderr.contents()).into_owned(),
    )
}

#[test]
fn stream_producers_finish_exactly_or_trap_with_a_diagnostic() {
    block(async {
        let (outcome, bytes, _) = read_frames("frames", (100_000, None)).await;
        outcome.unwrap();
        let expected: Vec<u8> = (0..100_000usize)
            .map(|i| (i.wrapping_mul(31).wrapping_add(7) & 0xff) as u8)
            .collect();
        assert!(
            bytes == expected,
            "clean stream delivered {} bytes",
            bytes.len()
        );

        let (outcome, bytes, stderr) = read_frames("frames-fail", (200_000, Some(20_480))).await;
        outcome.expect_err("a failed producer ended the stream cleanly");
        assert!(bytes.len() <= 20_480, "{}", bytes.len());
        // With the environment linked from the fixed context, the panic reaches stderr.
        assert!(
            stderr.contains("JavaScript stream producer failed: producer failed"),
            "{stderr}"
        );
    });
}

#[test]
fn stubs_keep_interfaces_they_use_for_types() {
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve
        .push_str(
            "deps.wit",
            r#"package test:deps;
            interface clock { record datetime { seconds: u64 } now: func() -> datetime; }
            interface files { use clock.{datetime}; stat: func() -> datetime; }
            world w { import clock; import files; export run: func() -> u32; }"#,
        )
        .unwrap();
    let world = resolve.select_world(&[package], Some("w")).unwrap();
    let mut module =
        wit_component::dummy_module(&resolve, world, wit_parser::ManglingAndAbi::Standard32);
    wit_component::embed_component_metadata(
        &mut module,
        &resolve,
        world,
        wit_component::StringEncoding::UTF8,
        false,
    )
    .unwrap();
    let component = wit_component::ComponentEncoder::default()
        .module(&module)
        .unwrap()
        .encode()
        .unwrap();

    let stubbed = BTreeSet::from(["test:deps/files".to_owned()]);
    let composed = link::stub_imports(&component, &stubbed).unwrap();
    let (resolve, world) = link::world(&composed).unwrap();
    assert_eq!(
        import_names(&resolve, world),
        BTreeSet::from(["test:deps/clock".to_owned()])
    );
    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&composed)
        .unwrap();
}

/// Deep native JSON parsing must hit QuickJS's guard before exhausting the Wasm shadow stack.
/// JavaScript calls themselves use dquickts's heap stack and do not test the native stack bound.
#[test]
fn deeply_nested_json_is_refused_without_poisoning_the_component() {
    let wit = "package tembra-fixture:stack; interface probe { parse: func(depth: u32) -> string; } world orb { export probe; }";
    let js = r#"
export const probe = {
  parse(depth) {
    try { JSON.parse("[".repeat(depth) + "0" + "]".repeat(depth)); return "ok"; }
    catch (error) { return `${error.name}:${error.message}`; }
  },
};
"#;
    let artifact = build(wit, js).unwrap();
    block(async {
        for interpreted in [false, true] {
            let engine = if interpreted {
                let mut config = Config::new();
                config.wasm_component_model(true);
                config.wasm_component_model_async(true);
                config.target("pulley64").unwrap();
                Engine::new(&config).unwrap()
            } else {
                engine()
            };
            let (mut store, instance, _) = instantiate_on(&artifact, |_| {}, engine).await;
            for depth in [64, 20_000, 64] {
                let result = call(
                    &mut store,
                    &instance,
                    "tembra-fixture:stack/probe",
                    "parse",
                    &[Val::U32(depth)],
                )
                .await
                .expect("native stack exhaustion must be a catchable JS error");
                if depth == 64 {
                    assert_eq!(result, Val::String("ok".into()));
                } else {
                    let Val::String(error) = result else {
                        panic!("unexpected parsing result")
                    };
                    assert!(error.contains("stack overflow"), "{error}");
                }
            }
        }
    });
}
