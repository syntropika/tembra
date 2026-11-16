//! A compiled consumer calls the native Orb that `#[tembra::orb]` generates: its values cross the
//! consumer's canonical ABI and the provider's codec both ways, as the boundary's JSON.
//!
//! The consumer is a real component encoded from a small core module, so it runs without a guest
//! toolchain: each function of its `relay` interface forwards its flat arguments to the same
//! function of the provider's `plotter` and returns what that one returns.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::rc::Rc;
use tembra::Host;
use tembra_executor_wasm::{Engine, Install, Limits};
use tembra_orb_native_fixture::calculator;

#[path = "support/components.rs"]
mod components;

const PLOTTER: &str = "example:calculator/plotter@1.0.0";
const RELAY: &str = "relay::example:calculator/relay@1.0.0";

/// The consumer's WIT: the provider's package, as the module derives it, with an interface that
/// relays the plotter's functions and a world that imports the plotter and exports the relay.
fn wit() -> String {
    format!(
        "{}
interface relay {{
  use plotter.{{point, heading, shape, stroke}};
  step: func(%from: point, heading: option<heading>, steps: u64) -> point;
  area: func(shape: shape) -> result<u64, string>;
  settled: func(area: result<u64, string>, fallback: u64) -> u64;
  redrawn: func(stroke: stroke, factor: u32) -> stroke;
  sorted: func(points: list<point>) -> tuple<list<option<heading>>, result<_, u32>>;
}}

world relay-consumer {{
  import plotter;
  export relay;
}}
",
        calculator::WIT
    )
}

/// Forwards each relay function's flat arguments to the plotter's. A result of more than one flat
/// value comes back through memory: the import writes it at offset 16 and the export returns that
/// offset. Strings and lists the host lowers into the consumer go to a bump allocator.
const RELAY_MODULE: &str = r#"(module
  (import "example:calculator/plotter@1.0.0" "step"
    (func $step (param i64 i64 i32 i32 i64 i32)))
  (import "example:calculator/plotter@1.0.0" "area"
    (func $area (param i32 i32 i32 i32)))
  (import "example:calculator/plotter@1.0.0" "settled"
    (func $settled (param i32 i64 i32 i64) (result i64)))
  (import "example:calculator/plotter@1.0.0" "redrawn"
    (func $redrawn (param i32 i32 i32 i32 i32 i32)))
  (import "example:calculator/plotter@1.0.0" "sorted"
    (func $sorted (param i32 i32 i32)))
  (memory (export "memory") 1)
  (global $next (mut i32) (i32.const 1024))
  (func (export "cabi_realloc") (param i32 i32 i32 i32) (result i32)
    (local $at i32)
    global.get $next
    local.get 2
    i32.add
    i32.const 1
    i32.sub
    i32.const 0
    local.get 2
    i32.sub
    i32.and
    local.tee $at
    local.get 3
    i32.add
    global.set $next
    local.get $at)
  (func (export "example:calculator/relay@1.0.0#step") (param i64 i64 i32 i32 i64) (result i32)
    local.get 0 local.get 1 local.get 2 local.get 3 local.get 4 i32.const 16 call $step
    i32.const 16)
  (func (export "example:calculator/relay@1.0.0#area") (param i32 i32 i32) (result i32)
    local.get 0 local.get 1 local.get 2 i32.const 16 call $area
    i32.const 16)
  (func (export "example:calculator/relay@1.0.0#settled") (param i32 i64 i32 i64) (result i64)
    local.get 0 local.get 1 local.get 2 local.get 3 call $settled)
  (func (export "example:calculator/relay@1.0.0#redrawn") (param i32 i32 i32 i32 i32) (result i32)
    local.get 0 local.get 1 local.get 2 local.get 3 local.get 4 i32.const 16 call $redrawn
    i32.const 16)
  (func (export "example:calculator/relay@1.0.0#sorted") (param i32 i32) (result i32)
    local.get 0 local.get 1 i32.const 16 call $sorted
    i32.const 16))"#;

/// A compiled consumer calls the native provider with records, an enum, a variant with a case
/// carrying a record, a flags, options, results and 64-bit integers, and gets back what a
/// portable caller of the provider gets: the codec reads what the consumer's lifting writes and
/// writes what its lowering reads, which the consumer's contract checks on the way.
#[test]
fn a_compiled_consumer_calls_the_native_orb_of_the_macro() {
    let engine = Engine::new(Limits::default()).unwrap();
    let artifact = components::encode(
        &wit(),
        "relay-consumer",
        RELAY_MODULE,
        "relay",
        &Default::default(),
    );
    let consumer = engine
        .load(
            &artifact,
            Install::default().requires(PLOTTER, "calculator", 2),
        )
        .unwrap();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![calculator::orb().unwrap(), consumer])
        .unwrap();
    let relay = Rc::new(host.get(RELAY, vec!["main".into()], "alice").unwrap());
    pool.run_until(relay.ready()).unwrap();
    let mut call = |method: &'static str, arguments: Value| {
        let relay = relay.clone();
        pool.run_until(host.root("alice", move |context| {
            async move {
                relay
                    .call_portable::<_, Value>(&context, method, arguments)
                    .await
            }
            .boxed_local()
        }))
        .unwrap_or_else(|error| panic!("{method}: {error}"))
    };
    let far = 9_007_199_254_740_993_i64;
    for (method, arguments, result) in [
        (
            "step",
            json!([{ "x": 1, "y": far }, "north", 2]),
            json!({ "x": 1, "y": far + 2 }),
        ),
        (
            "step",
            json!([{ "x": -5, "y": 0 }, null, u64::MAX]),
            json!({ "x": -5, "y": 0 }),
        ),
        (
            "area",
            json!([{ "tag": "rect", "val": { "width": 2, "height": 3 } }]),
            json!({ "tag": "ok", "val": 6 }),
        ),
        (
            "area",
            json!([{ "tag": "dot" }]),
            json!({ "tag": "err", "val": "a dot has no area" }),
        ),
        (
            "settled",
            json!([{ "tag": "err", "val": "none" }, 4]),
            json!(4),
        ),
        (
            "redrawn",
            json!([{
                "shape": { "tag": "square", "val": 2 },
                "pen": { "dashedLine": true }
            }, 3]),
            json!({ "shape": { "tag": "square", "val": 6 }, "pen": { "filled": true } }),
        ),
        (
            "sorted",
            json!([[{ "x": 0, "y": -2 }, { "x": 3, "y": 0 }, { "x": 1, "y": 1 }]]),
            json!([["south", "east", null], { "tag": "err", "val": 1 }]),
        ),
        ("sorted", json!([[]]), json!([[], { "tag": "ok" }])),
    ] {
        assert_eq!(call(method, arguments), result, "{method}");
    }
    relay.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}
