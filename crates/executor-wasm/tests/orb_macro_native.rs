//! A compiled consumer calls the native Orb that `#[tembra::orb]` generates: its values cross the
//! consumer's canonical ABI and the provider's codec both ways, as the boundary's JSON, and it
//! holds the Orb's WIT resources through ordinary handles. The other way round, a native Orb of
//! the macro calls a compiled provider through its import, and contributes to the hook a compiled
//! Orb declares.
//!
//! Each compiled Orb is a real component encoded from a small core module, so it runs without a
//! guest toolchain: each function of the consumer's `relay` interface forwards its flat arguments
//! to the same function of the provider's `plotter` and returns what that one returns, and its
//! `holder` constructs, calls and drops one of the provider's accumulators, and two it receives
//! nested in the result of the provider's `open`; the meter answers its async functions at once,
//! and the gate runs its hook over each value it admits.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::rc::Rc;
use tembra::{Host, Provider};
use tembra_executor_wasm::{Engine, Install, Limits};
use tembra_orb_native_fixture::{
    calculator::{self, accumulators::Accumulators},
    filter, metering,
};

#[path = "support/components.rs"]
mod components;

const PLOTTER: &str = "example:calculator/plotter@1.0.0";
const RELAY: &str = "relay::example:calculator/relay@1.0.0";
const ACCUMULATORS: &str = "example:calculator/accumulators@1.0.0";
const HOLDER: &str = "holder::example:calculator/holder@1.0.0";
const READINGS: &str = "metering::example:metering/readings@1.0.0";

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

/// The holder's WIT: the provider's package, with an interface whose function holds one of the
/// provider's accumulators, and a world that imports the accumulators and exports the holder.
fn holder_wit() -> String {
    format!(
        "{}
interface holder {{
  run: func(start: s64, value: s64) -> s64;
  second: func(first: s64, second: s64, value: s64) -> s64;
}}

world holder-consumer {{
  import accumulators;
  export holder;
}}
",
        calculator::WIT
    )
}

/// `run` constructs an accumulator at its start, adds its value through the handle, drops the
/// handle and returns the total `add` answered. `second` opens an accumulator at each of its two
/// starts through the service's `open`, which hands them out nested, as a list in a tuple: the
/// starts go at offset 64, and `open` writes the list of handles, which the host's one allocation
/// puts at offset 1024, and the count from offset 16, trapping `second` unless both say 2. It adds
/// its value through the second handle, drops both and returns the total `add` answered.
const HOLDER_MODULE: &str = r#"(module
  (import "example:calculator/accumulators@1.0.0" "[constructor]accumulator"
    (func $new (param i64) (result i32)))
  (import "example:calculator/accumulators@1.0.0" "[method]accumulator.add"
    (func $add (param i32 i64) (result i64)))
  (import "example:calculator/accumulators@1.0.0" "[resource-drop]accumulator"
    (func $drop (param i32)))
  (import "example:calculator/accumulators@1.0.0" "open"
    (func $open (param i32 i32 i32)))
  (memory (export "memory") 1)
  (func (export "cabi_realloc") (param i32 i32 i32 i32) (result i32) i32.const 1024)
  (func (export "example:calculator/holder@1.0.0#run") (param $start i64) (param $value i64)
    (result i64)
    (local $accumulator i32)
    (local $total i64)
    local.get $start
    call $new
    local.set $accumulator
    local.get $accumulator
    local.get $value
    call $add
    local.set $total
    local.get $accumulator
    call $drop
    local.get $total)
  (func (export "example:calculator/holder@1.0.0#second")
    (param $first i64) (param $second i64) (param $value i64) (result i64)
    (local $handles i32)
    (local $total i64)
    i32.const 64
    local.get $first
    i64.store
    i32.const 72
    local.get $second
    i64.store
    i32.const 64
    i32.const 2
    i32.const 16
    call $open
    i32.const 20
    i32.load
    i32.const 2
    i32.ne
    if unreachable end
    i32.const 24
    i32.load
    i32.const 2
    i32.ne
    if unreachable end
    i32.const 16
    i32.load
    local.set $handles
    local.get $handles
    i32.load offset=4
    local.get $value
    call $add
    local.set $total
    local.get $handles
    i32.load
    call $drop
    local.get $handles
    i32.load offset=4
    call $drop
    local.get $total))"#;

/// A compiled consumer constructs a WIT resource of the native Orb, calls its method through the
/// handle and drops it: the method reads as its `self` the value the constructor issued, and the
/// value closes once the consumer's call has settled. It also receives two of them nested in a
/// service function's result, each handle naming its own value.
#[test]
fn a_compiled_consumer_constructs_calls_and_drops_a_native_resource() {
    let engine = Engine::new(Limits::default()).unwrap();
    let artifact = components::encode(
        &holder_wit(),
        "holder-consumer",
        HOLDER_MODULE,
        "holder",
        &Default::default(),
    );
    let consumer = engine
        .load(
            &artifact,
            Install::default().requires(ACCUMULATORS, "calculator", 2),
        )
        .unwrap();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![calculator::orb().unwrap(), consumer])
        .unwrap();
    let holder = Rc::new(host.get(HOLDER, vec!["main".into()], "alice").unwrap());
    pool.run_until(holder.ready()).unwrap();
    let dropped = Accumulators::dropped();
    let mut call = |method: &'static str, arguments: Value| {
        let called = holder.clone();
        let total = pool
            .run_until(host.root("alice", move |context| {
                async move {
                    called
                        .call_portable::<_, i64>(&context, method, arguments)
                        .await
                }
                .boxed_local()
            }))
            .unwrap_or_else(|error| panic!("{method}: {error}"));
        pool.run_until(host.settled());
        total
    };
    assert_eq!(call("run", json!([5, 2])), 7);
    assert_eq!(Accumulators::dropped(), dropped + 1, "dropped once");
    // The second start is 7: a handle naming the first value would answer 7.
    assert_eq!(call("second", json!([5, 7, 2])), 9);
    assert_eq!(Accumulators::dropped(), dropped + 3, "each dropped once");
    holder.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(Accumulators::dropped(), dropped + 3);
    assert_eq!(engine.instances(), 0);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}

/// The meter's WIT, the package the native fixture's `metering` imports, with a world exporting
/// its gauge.
fn meter_wit() -> String {
    format!(
        "{}
world meter {{
  export gauge;
}}
",
        include_str!("../../orb-macro/native-fixture/wit/deps/meter/meter.wit")
    )
}

/// A compiled meter: each async-lifted function returns at once, with `task.return`, the total of
/// the readings `add` was given, kept in a global.
const METER_MODULE: &str = r#"(module
  (import "[export]example:meter/gauge@1.0.0" "[task-return]add" (func $added (param i64)))
  (import "[export]example:meter/gauge@1.0.0" "[task-return]total" (func $totalled (param i64)))
  (memory (export "memory") 1)
  (global $total (mut i64) (i64.const 0))
  (func (export "cabi_realloc") (param i32 i32 i32 i32) (result i32) i32.const 1024)
  (func (export "[async-lift]example:meter/gauge@1.0.0#add")
    (param $amount i32) (param $times i32) (result i32)
    global.get $total
    local.get $amount
    i64.extend_i32_u
    local.get $times
    i64.extend_i32_u
    i64.mul
    i64.add
    global.set $total
    global.get $total
    call $added
    i32.const 0)
  (func (export "[callback][async-lift]example:meter/gauge@1.0.0#add")
    (param i32 i32 i32) (result i32)
    i32.const 0)
  (func (export "[async-lift]example:meter/gauge@1.0.0#total") (result i32)
    global.get $total
    call $totalled
    i32.const 0)
  (func (export "[callback][async-lift]example:meter/gauge@1.0.0#total")
    (param i32 i32 i32) (result i32)
    i32.const 0))"#;

/// A native consumer that `#[tembra::orb]` generates calls a compiled provider through its import
/// of an interface of `async func`s: the kernel admits the consumer's requirement against the
/// signature it computes for the component's contract, and each call's arguments and result cross
/// the consumer's codec and the component's canonical ABI, a record of the interface included.
#[test]
fn a_native_consumer_calls_a_compiled_provider_through_its_import() {
    let engine = Engine::new(Limits::default()).unwrap();
    let artifact = components::encode(
        &meter_wit(),
        "meter",
        METER_MODULE,
        "meters",
        &Default::default(),
    );
    let provider = engine.load(&artifact, Install::default()).unwrap();
    let consumer = metering::orb_named(
        "metering",
        1,
        metering::Providers {
            gauge: Provider::new("meters", 1),
            gauge_ref: Provider::new("meters", 1),
        },
    )
    .unwrap();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![provider, consumer]).unwrap();
    let readings = Rc::new(host.get(READINGS, vec!["main".into()], "alice").unwrap());
    pool.run_until(readings.ready()).unwrap();
    for (amounts, total) in [(json!([2, 3]), 5), (json!([4]), 9)] {
        let called = readings.clone();
        let fed = pool
            .run_until(host.root("alice", move |context| {
                async move {
                    called
                        .call_portable::<_, Value>(&context, "feed", json!([amounts]))
                        .await
                }
                .boxed_local()
            }))
            .unwrap();
        assert_eq!(fed, json!(total));
    }
    readings.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}

const GUARD: &str = "example:gate/guard@1.0.0";
const ADMISSIONS: &str = "gate::example:gate/admissions@1.0.0";

/// The gate's WIT, whose hook the native fixture's `filter` contributes to, with a world that
/// declares the hook and exports the gate's service.
fn gate_wit() -> String {
    format!(
        "{}
world gate {{
  import guard;
  export guard;
  export admissions;
}}
",
        include_str!("../../orb-macro/native-fixture/wit/deps/gate/gate.wit")
    )
}

/// A compiled gate: the export of its declared hook traps, because contributions answer it, and
/// `admit` hands a fresh admission of its value to the hook's import, which runs the plan, and
/// returns what the plan composed. The admission crosses in memory, at offset 16.
const GATE_MODULE: &str = r#"(module
  (import "example:gate/guard@1.0.0" "apply" (func $run (param i32 i32 i32)))
  (memory (export "memory") 1)
  (func (export "cabi_realloc") (param i32 i32 i32 i32) (result i32) i32.const 1024)
  (func (export "example:gate/guard@1.0.0#apply") (param i32 i32) (result i32) unreachable)
  (func (export "example:gate/admissions@1.0.0#admit") (param i32) (result i32)
    local.get 0 i32.const 0 i32.const 16 call $run
    i32.const 16))"#;

/// A native contribution that `#[tembra::orb]` generates joins the hook a compiled Orb declares:
/// the kernel admits its requirement against the signature it computes for the gate's hook, and
/// each run of the plan reads the admission with the contribution's codec, runs the author's
/// function in the contributor's root, under the principal of the call and on no object, as in a
/// component, and writes its answer back to the gate.
#[test]
fn a_native_contribution_joins_the_hook_a_compiled_orb_declares() {
    let engine = Engine::new(Limits::default()).unwrap();
    let artifact = components::encode(
        &gate_wit(),
        "gate",
        GATE_MODULE,
        "gate",
        &tembra_contracts::ComponentRoles {
            hooks: [(
                GUARD.to_owned(),
                tembra_contracts::HookComposition::Waterfall,
            )]
            .into(),
            ..Default::default()
        },
    );
    let gate = engine.load(&artifact, Install::default()).unwrap();
    let contributor = filter::orb(filter::Providers {
        guard: Provider::new("gate", 1),
    })
    .unwrap();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![gate, contributor]).unwrap();
    let admissions = Rc::new(host.get(ADMISSIONS, vec!["main".into()], "alice").unwrap());
    pool.run_until(admissions.ready()).unwrap();
    filter::seen();
    for (value, admitted) in [
        (5, json!({ "value": 5, "blocked": false })),
        (-3, json!({ "value": -3, "blocked": true })),
    ] {
        let called = admissions.clone();
        let answer = pool
            .run_until(host.root("alice", move |context| {
                async move {
                    called
                        .call_portable::<_, Value>(&context, "admit", json!([value]))
                        .await
                }
                .boxed_local()
            }))
            .unwrap();
        assert_eq!(answer, admitted, "{value}");
    }
    assert_eq!(
        filter::seen(),
        ["5 for alice on []@host", "-3 for alice on []@host"]
    );
    admissions.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}
