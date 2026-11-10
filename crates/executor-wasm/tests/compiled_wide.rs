//! WIT `u64`/`s64` values cross compiled components and typed Rust Orbs exactly.
//!
//! Native callers and providers see ordinary exact numbers; executor packets carry canonical
//! decimal strings. The components are encoded from small core modules, so the test needs no
//! guest toolchain, and each case runs on Cranelift and on Pulley.
use futures::{FutureExt, executor::LocalPool};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::rc::Rc;
use tembra::{Host, Orb, Reference, Service};
use tembra_executor_wasm::{Contract, Engine, Install, Limits};

const WIT: &str = r#"
package example:wide@1.0.0;

interface values {
    record sample {
        small: u32,
        large: u64,
        signed: s64,
        maybe: option<s64>,
        label: string,
        counts: list<u64>,
    }
    shift: func(value: sample) -> sample;
}
interface relay {
    use values.{sample};
    forward: func(value: sample) -> sample;
}

world provider { export values; }
world consumer { import values; export relay; }
"#;

const VALUES: &str = "example:wide/values@1.0.0";
const RELAY: &str = "example:wide/relay@1.0.0";

/// Bump allocator shared by both modules; the host places strings and lists through it.
const REALLOC: &str = r#"
  (global $next (mut i32) (i32.const 1024))
  (func (export "cabi_realloc") (param i32 i32 i32 i32) (result i32)
    (local $at i32)
    global.get $next
    local.get 2
    i32.const 1
    i32.sub
    i32.add
    i32.const 0
    local.get 2
    i32.sub
    i32.and
    local.tee $at
    local.get 3
    i32.add
    global.set $next
    local.get $at)"#;

/// `shift` returns `large - 1` and `signed + 1` with wrapping 64-bit arithmetic, and every
/// other field unchanged. The record (size 56, align 8) is written at offset 256.
fn provider_module() -> String {
    format!(
        r#"(module
  (memory (export "memory") 1)
  {REALLOC}
  (func (export "example:wide/values@1.0.0#shift")
    (param $small i32) (param $large i64) (param $signed i64)
    (param $some i32) (param $maybe i64)
    (param $label i32) (param $length i32) (param $counts i32) (param $count i32)
    (result i32)
    i32.const 256 local.get $small i32.store
    i32.const 264 local.get $large i64.const 1 i64.sub i64.store
    i32.const 272 local.get $signed i64.const 1 i64.add i64.store
    i32.const 280 local.get $some i32.store8
    i32.const 288 local.get $maybe i64.store
    i32.const 296 local.get $label i32.store
    i32.const 300 local.get $length i32.store
    i32.const 304 local.get $counts i32.store
    i32.const 308 local.get $count i32.store
    i32.const 256))"#
    )
}

/// `forward` passes its record to the synchronous `shift` import and returns its result.
fn consumer_module() -> String {
    format!(
        r#"(module
  (import "example:wide/values@1.0.0" "shift"
    (func $shift (param i32 i64 i64 i32 i64 i32 i32 i32 i32 i32)))
  (memory (export "memory") 1)
  {REALLOC}
  (func (export "example:wide/relay@1.0.0#forward")
    (param i32 i64 i64 i32 i64 i32 i32 i32 i32) (result i32)
    local.get 0 local.get 1 local.get 2 local.get 3 local.get 4
    local.get 5 local.get 6 local.get 7 local.get 8
    i32.const 512
    call $shift
    i32.const 512))"#
    )
}

/// The typed Rust spelling of `sample`: ordinary integers, no wrappers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Sample {
    small: u32,
    large: u64,
    signed: i64,
    maybe: Option<i64>,
    label: String,
    counts: Vec<u64>,
}

impl Sample {
    fn shifted(&self) -> Self {
        Self {
            large: self.large.wrapping_sub(1),
            signed: self.signed.wrapping_add(1),
            ..self.clone()
        }
    }
}

/// Values beyond 2^53 and at every 64-bit edge, next to 32-bit and digit-string neighbours.
fn samples() -> Vec<Sample> {
    vec![
        Sample {
            small: u32::MAX,
            large: u64::MAX,
            signed: i64::MIN,
            maybe: Some(i64::MAX),
            label: "18446744073709551615".into(),
            counts: vec![0, 9_007_199_254_740_993, u64::MAX],
        },
        Sample {
            small: 0,
            large: 0,
            signed: i64::MAX,
            maybe: None,
            label: String::new(),
            counts: vec![],
        },
        Sample {
            small: 7,
            large: 9_007_199_254_740_993,
            signed: -9_007_199_254_740_993,
            maybe: Some(i64::MIN),
            label: "123".into(),
            counts: vec![1],
        },
    ]
}

fn artifact(world: &str, module: &str, name: &str) -> Vec<u8> {
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve.push_str("wide.wit", WIT).unwrap();
    let selected = resolve.select_world(&[package], Some(world)).unwrap();
    let mut core = wat::parse_str(module).unwrap();
    wit_component::embed_component_metadata(
        &mut core,
        &resolve,
        selected,
        wit_component::StringEncoding::UTF8,
        false,
    )
    .unwrap();
    let component = wit_component::ComponentEncoder::default()
        .validate(true)
        .module(&core)
        .unwrap()
        .encode()
        .unwrap();
    Contract::from_source("wide.wit", WIT, Some(world))
        .unwrap()
        .component_artifact(&component, name, 1, &Default::default())
        .unwrap()
}

/// A trusted Rust provider of `values` with a typed method.
fn native_provider() -> Orb {
    let service = Contract::from_source("wide.wit", WIT, Some("provider"))
        .unwrap()
        .plan("values", 1, &Default::default())
        .unwrap()
        .orb
        .services
        .remove(0);
    Orb::new("values", 1).service(
        Service::new(service.name, |_| Ok(()))
            .signature(service.signature)
            .method("shift", |_, _, (value,): (Sample,)| async move {
                Ok(value.shifted())
            }),
    )
}

fn engine(interpreted: bool) -> Engine {
    let limits = Limits {
        timeout_ms: 30_000,
        ..Limits::default()
    };
    if interpreted {
        Engine::interpreted(limits)
    } else {
        Engine::new(limits)
    }
    .unwrap()
}

fn reference(pool: &mut LocalPool, host: &Host, service: &str) -> Rc<Reference> {
    let reference = Rc::new(host.get(service, vec!["main".into()], "alice").unwrap());
    pool.run_until(reference.ready()).unwrap();
    reference
}

fn call(
    pool: &mut LocalPool,
    host: &Host,
    reference: &Rc<Reference>,
    method: &'static str,
    arguments: Value,
) -> tembra::Result<Value> {
    let reference = reference.clone();
    pool.run_until(host.root("alice", move |context| {
        async move {
            reference
                .call::<_, Value>(&context, method, arguments)
                .await
        }
        .boxed_local()
    }))
}

/// Native typed values in, through `service`'s `method`, and typed values out.
fn typed(
    pool: &mut LocalPool,
    host: &Host,
    reference: &Rc<Reference>,
    method: &'static str,
    sample: &Sample,
) -> Sample {
    let arguments = serde_json::to_value((sample,)).unwrap();
    let result = call(pool, host, reference, method, arguments).unwrap();
    serde_json::from_value(result).unwrap()
}

/// A typed Rust caller reaches a compiled provider; the packet strings never surface.
fn native_to_compiled(interpreted: bool) {
    let engine = engine(interpreted);
    let provider = engine
        .load(
            &artifact("provider", &provider_module(), "values"),
            Install::default(),
        )
        .unwrap();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![provider]).unwrap();
    let values = reference(&mut pool, &host, &format!("values::{VALUES}"));
    for sample in samples() {
        assert_eq!(
            typed(&mut pool, &host, &values, "shift", &sample),
            sample.shifted()
        );
    }
    // The exact JSON the native side sees is numeric, never a decimal string.
    let result = call(
        &mut pool,
        &host,
        &values,
        "shift",
        serde_json::to_value((&samples()[0],)).unwrap(),
    )
    .unwrap();
    assert_eq!(result["large"].to_string(), "18446744073709551614");
    assert_eq!(result["signed"].to_string(), "-9223372036854775807");
    assert_eq!(result["counts"][2].to_string(), "18446744073709551615");
    assert_eq!(result["label"], json!("18446744073709551615"));
    // Native callers use numbers only; a decimal string is not an alternative spelling.
    let mut spelled = serde_json::to_value((&samples()[0],)).unwrap();
    spelled[0]["large"] = json!("18446744073709551615");
    let error = call(&mut pool, &host, &values, "shift", spelled).unwrap_err();
    assert!(error.to_string().contains("does not match U64"), "{error}");
    let mut fraction = serde_json::to_value((&samples()[1],)).unwrap();
    fraction[0]["signed"] = serde_json::from_str("1.0").unwrap();
    assert!(call(&mut pool, &host, &values, "shift", fraction).is_err());
    values.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}

/// A compiled consumer's import reaches a typed Rust provider and a compiled provider.
fn compiled_to_providers(interpreted: bool) {
    let engine = engine(interpreted);
    for compiled in [false, true] {
        let provider = if compiled {
            engine
                .load(
                    &artifact("provider", &provider_module(), "values"),
                    Install::default(),
                )
                .unwrap()
        } else {
            native_provider()
        };
        let consumer = engine
            .load(
                &artifact("consumer", &consumer_module(), "consumer"),
                Install::default().requires(VALUES, "values", 1),
            )
            .unwrap();
        let mut pool = LocalPool::new();
        let host = Host::new(pool.spawner());
        host.install(vec![provider, consumer]).unwrap();
        let relay = reference(&mut pool, &host, &format!("consumer::{RELAY}"));
        for sample in samples() {
            assert_eq!(
                typed(&mut pool, &host, &relay, "forward", &sample),
                sample.shifted(),
                "compiled provider: {compiled}"
            );
        }
        relay.release().unwrap();
        pool.run_until(host.close()).unwrap();
        assert_eq!(engine.instances(), 0);
        assert!(host.errors().is_empty(), "{:?}", host.errors());
    }
}

#[test]
fn native_typed_values_reach_a_compiled_provider_exactly() {
    native_to_compiled(false);
}

#[test]
fn native_typed_values_reach_a_pulley_provider_exactly() {
    native_to_compiled(true);
}

#[test]
fn compiled_imports_reach_typed_and_compiled_providers_exactly() {
    compiled_to_providers(false);
}

#[test]
fn pulley_imports_reach_typed_and_compiled_providers_exactly() {
    compiled_to_providers(true);
}
