//! WIT `u64`/`s64` values cross real source Orbs exactly, against typed Rust and compiled peers.
//!
//! The provider and relay source Orbs are the prepared TypeScript precision fixtures
//! (`<TEMBRA_WASM_FIXTURES>/precision.wasm` and `precision-relay.wasm`). Their WIT is copied
//! below; installation compares every signature, so a drifted copy is refused. Compiled peers are
//! encoded from core modules, and the source hook Orbs are the prepared SDK fixtures
//! `tests/source/test-tally-{provider,extension}`. Every case runs on Cranelift and Pulley.
#[path = "support/source.rs"]
mod source;
use futures::{FutureExt, executor::LocalPool};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::rc::Rc;
use tembra::{Hook, Host, Orb, Reference, Service, core::Requirement};
use tembra_executor_wasm::{Contract, Engine, Install, Limits};

/// `interorb-fixtures/sources/precision-provider/contract.wit`, verbatim.
const WIT: &str = r#"package example:precision@1.0.0;

/// Exact 64-bit integers through every WIT container the portable profile carries.
interface numbers {
    type big = u64;
    record sample {
        id: big,
        values: list<s64>,
        maybe: option<u64>,
        small: u32,
        label: string,
    }
    variant pick {
        signed(s64),
        none,
    }
    add: func(a: u64, b: s64) -> u64;
    echo: func(s: sample, p: pick, t: tuple<u64, string>) -> result<sample, s64>;
}

/// The same calls, relayed by a second Orb through its WIT import.
interface relay {
    use numbers.{sample, pick};
    add: func(a: u64, b: s64) -> u64;
    echo: func(s: sample, p: pick, t: tuple<u64, string>) -> result<sample, s64>;
}

world provider {
    export numbers;
}

world consumer {
    import numbers;
    export relay;
}
"#;

const NUMBERS: &str = "example:precision/numbers@1.0.0";
const RELAY: &str = "example:precision/relay@1.0.0";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Sample {
    id: u64,
    values: Vec<i64>,
    maybe: Option<u64>,
    small: u32,
    label: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "tag", content = "val", rename_all = "lowercase")]
enum Pick {
    Signed(i64),
    None,
}

/// `result<sample, s64>` in the portable tagged spelling.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "tag", content = "val", rename_all = "lowercase")]
enum Echoed {
    Ok(Sample),
    Err(i64),
}

/// The fixture's `echo`: a negative signed pick is the error; otherwise the tuple's integer
/// becomes the id, values reverse, a signed pick replaces `maybe`, and the labels join.
fn echo(sample: &Sample, pick: Pick, tuple: &(u64, String)) -> Echoed {
    match pick {
        Pick::Signed(value) if value < 0 => Echoed::Err(value),
        _ => Echoed::Ok(Sample {
            id: tuple.0,
            values: sample.values.iter().rev().copied().collect(),
            maybe: match pick {
                Pick::Signed(value) => Some(value as u64),
                Pick::None => sample.maybe,
            },
            small: sample.small,
            label: format!("{}:{}", sample.label, tuple.1),
        }),
    }
}

/// Values beyond 2^53 and at every 64-bit edge, next to 32-bit and digit-string neighbours.
fn echoes() -> Vec<(Sample, Pick, (u64, String))> {
    let full = Sample {
        id: u64::MAX,
        values: vec![i64::MIN, -1, 0, i64::MAX, 9_007_199_254_740_993],
        maybe: Some(u64::MAX),
        small: u32::MAX,
        label: "18446744073709551615".into(),
    };
    let empty = Sample {
        id: 0,
        values: vec![],
        maybe: None,
        small: 0,
        label: String::new(),
    };
    vec![
        (
            full.clone(),
            Pick::Signed(i64::MAX),
            (u64::MAX - 1, "9007199254740993".into()),
        ),
        (
            full.clone(),
            Pick::None,
            (9_007_199_254_740_993, "x".into()),
        ),
        (empty.clone(), Pick::None, (0, String::new())),
        (empty, Pick::Signed(0), (1, "123".into())),
        (full.clone(), Pick::Signed(i64::MIN), (0, "err".into())),
        (full, Pick::Signed(-1), (0, "err".into())),
    ]
}

/// In-range `add` cases and their exact results.
const SUMS: [(u64, i64, u64); 4] = [
    (u64::MAX - 5, 5, u64::MAX),
    (9_007_199_254_740_993, -9_007_199_254_740_993, 0),
    (0, i64::MAX, i64::MAX as u64),
    (u64::MAX, i64::MIN, u64::MAX - (1 << 63)),
];

/// `add` results outside `u64`, refused by the source guest and by the typed Rust provider.
const OVERFLOWS: [(u64, i64); 2] = [(u64::MAX, 1), (0, -1)];

/// Bump allocator; the host places strings and lists through it.
const REALLOC: &str = r#"
  (global $next (mut i32) (i32.const 1024))
  (func $alloc (param $align i32) (param $size i32) (result i32)
    (local $at i32)
    global.get $next local.get $align i32.const 1 i32.sub i32.add
    i32.const 0 local.get $align i32.sub i32.and
    local.tee $at local.get $size i32.add global.set $next
    local.get $at)
  (func (export "cabi_realloc") (param i32 i32 i32 i32) (result i32)
    local.get 2 local.get 3 call $alloc)"#;

/// The fixture's semantics in core Wasm, for in-range `add`. `result<sample, s64>` is written at
/// 256: discriminant at 256, payload at 264 (`id` 264, `values` 272/276, `maybe` 280/288,
/// `small` 296, `label` 300/304).
fn compiled_provider() -> String {
    format!(
        r#"(module
  (memory (export "memory") 2)
  {REALLOC}
  (func (export "example:precision/numbers@1.0.0#add") (param i64 i64) (result i64)
    local.get 0 local.get 1 i64.add)
  (func (export "example:precision/numbers@1.0.0#echo")
    (param $id i64) (param $vp i32) (param $vl i32) (param $md i32) (param $mv i64)
    (param $small i32) (param $lp i32) (param $ll i32) (param $pd i32) (param $pv i64)
    (param $t0 i64) (param $tp i32) (param $tl i32) (result i32)
    (local $np i32) (local $i i32) (local $label i32)
    local.get $pd i32.eqz
    if
      local.get $pv i64.const 0 i64.lt_s
      if
        i32.const 256 i32.const 1 i32.store8
        i32.const 264 local.get $pv i64.store
        i32.const 256 return
      end
      i32.const 1 local.set $md
      local.get $pv local.set $mv
    end
    i32.const 8 local.get $vl i32.const 3 i32.shl call $alloc local.set $np
    block $done
      loop $copy
        local.get $i local.get $vl i32.ge_u br_if $done
        local.get $np local.get $i i32.const 3 i32.shl i32.add
        local.get $vp
        local.get $vl i32.const 1 i32.sub local.get $i i32.sub i32.const 3 i32.shl
        i32.add i64.load
        i64.store
        local.get $i i32.const 1 i32.add local.set $i
        br $copy
      end
    end
    i32.const 1 local.get $ll local.get $tl i32.add i32.const 1 i32.add call $alloc
    local.set $label
    local.get $label local.get $lp local.get $ll memory.copy
    local.get $label local.get $ll i32.add i32.const 58 i32.store8
    local.get $label local.get $ll i32.add i32.const 1 i32.add local.get $tp local.get $tl
    memory.copy
    i32.const 256 i32.const 0 i32.store8
    i32.const 264 local.get $t0 i64.store
    i32.const 272 local.get $np i32.store
    i32.const 276 local.get $vl i32.store
    i32.const 280 local.get $md i32.store8
    i32.const 288 local.get $mv i64.store
    i32.const 296 local.get $small i32.store
    i32.const 300 local.get $label i32.store
    i32.const 304 local.get $ll local.get $tl i32.add i32.const 1 i32.add i32.store
    i32.const 256))"#
    )
}

/// A compiled relay: each export forwards to the synchronous `numbers` import.
fn compiled_relay() -> String {
    format!(
        r#"(module
  (import "example:precision/numbers@1.0.0" "add" (func $add (param i64 i64) (result i64)))
  (import "example:precision/numbers@1.0.0" "echo"
    (func $echo (param i64 i32 i32 i32 i64 i32 i32 i32 i32 i64 i64 i32 i32 i32)))
  (memory (export "memory") 2)
  {REALLOC}
  (func (export "example:precision/relay@1.0.0#add") (param i64 i64) (result i64)
    local.get 0 local.get 1 call $add)
  (func (export "example:precision/relay@1.0.0#echo")
    (param i64 i32 i32 i32 i64 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)
    local.get 0 local.get 1 local.get 2 local.get 3 local.get 4 local.get 5 local.get 6
    local.get 7 local.get 8 local.get 9 local.get 10 local.get 11 local.get 12
    i32.const 256
    call $echo
    i32.const 256))"#
    )
}

fn component(wit: &str, world: &str, module: &str, name: &str) -> Vec<u8> {
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve.push_str("contract.wit", wit).unwrap();
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
    Contract::from_source("contract.wit", wit, Some(world))
        .unwrap()
        .component_artifact(&component, name, 1, &Default::default())
        .unwrap()
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

/// A typed Rust provider of `numbers`: ordinary `u64`/`i64` arguments and results.
fn native_provider() -> Orb {
    let service = Contract::from_source("contract.wit", WIT, Some("provider"))
        .unwrap()
        .plan("precision", 1, &Default::default())
        .unwrap()
        .orb
        .services
        .remove(0);
    Orb::new("precision", 1).service(
        Service::new(service.name, |_| Ok(()))
            .signature(service.signature)
            .method("add", |_, _, (a, b): (u64, i64)| async move {
                a.checked_add_signed(b)
                    .ok_or_else(|| tembra::Error::invalid("range", "u64 result out of range"))
            })
            .method(
                "echo",
                |_, _, (sample, pick, tuple): (Sample, Pick, (u64, String))| async move {
                    Ok(echo(&sample, pick, &tuple))
                },
            ),
    )
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Native,
    Source,
    Compiled,
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

/// The portable entry every foreign caller uses: one JSON argument list, exact numbers.
fn portable_call(
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
                .call_portable::<_, serde_json::Value>(&context, method, arguments)
                .await
        }
        .boxed_local()
    }))
}

/// Install `provider` and, unless `relay` is `None`, a relay of that kind; then check every
/// case from a typed Rust caller.
fn leg(engine: &Engine, provider: Kind, relay: Option<Kind>) {
    let label = format!("{provider:?} provider, {relay:?} relay");
    let numbers = match provider {
        Kind::Native => native_provider(),
        Kind::Source => engine
            .load(&source::fixture("precision"), Install::default())
            .unwrap(),
        Kind::Compiled => engine
            .load(
                &component(WIT, "provider", &compiled_provider(), "precision"),
                Install::default(),
            )
            .unwrap(),
    };
    let requires = || Install::default().requires(NUMBERS, "precision", 1);
    let mut orbs = vec![numbers];
    let service = match relay {
        None => format!("precision::{NUMBERS}"),
        Some(Kind::Source) => {
            orbs.push(
                engine
                    .load(&source::fixture("precision-relay"), requires())
                    .unwrap(),
            );
            format!("relay::{RELAY}")
        }
        Some(Kind::Compiled) => {
            let bytes = component(WIT, "consumer", &compiled_relay(), "relay");
            orbs.push(engine.load(&bytes, requires()).unwrap());
            format!("relay::{RELAY}")
        }
        Some(Kind::Native) => unreachable!("native relays are covered by the kernel tests"),
    };
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(orbs).unwrap();
    let target = reference(&mut pool, &host, &service);
    for (a, b, sum) in SUMS {
        let result = call(&mut pool, &host, &target, "add", json!([a, b])).unwrap();
        // The native caller sees an exact JSON number, never a decimal string.
        assert!(result.is_number(), "{label}: {result}");
        assert_eq!(
            result.to_string(),
            sum.to_string(),
            "{label}: add({a}, {b})"
        );
        assert_eq!(serde_json::from_value::<u64>(result).unwrap(), sum);
    }
    for (sample, pick, tuple) in echoes() {
        let arguments = serde_json::to_value((&sample, pick, &tuple)).unwrap();
        let result = call(&mut pool, &host, &target, "echo", arguments).unwrap();
        let typed: Echoed = serde_json::from_value(result).unwrap();
        assert_eq!(typed, echo(&sample, pick, &tuple), "{label}: {pick:?}");
    }
    // A compiled provider wraps instead of refusing, and a refused import would trap a compiled
    // relay; overflow is checked where the refusal is a typed error.
    if provider != Kind::Compiled && relay != Some(Kind::Compiled) {
        for (a, b) in OVERFLOWS {
            assert!(
                call(&mut pool, &host, &target, "add", json!([a, b])).is_err(),
                "{label}: add({a}, {b}) overflows u64"
            );
        }
    }
    // A decimal string is not an alternative native spelling.
    assert!(
        call(&mut pool, &host, &target, "add", json!(["1", 1])).is_err(),
        "{label}"
    );
    target.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0, "{label}");
    assert!(host.errors().is_empty(), "{label}: {:?}", host.errors());
}

fn calls(interpreted: bool) {
    let engine = engine(interpreted);
    // Native typed -> source provider.
    leg(&engine, Kind::Source, None);
    // Source relay -> typed native, source and compiled providers.
    for provider in [Kind::Native, Kind::Source, Kind::Compiled] {
        leg(&engine, provider, Some(Kind::Source));
    }
    // Compiled relay -> source provider.
    leg(&engine, Kind::Source, Some(Kind::Compiled));
}

#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn source_64_bit_calls_are_exact_against_native_source_and_compiled_peers() {
    calls(false);
}

#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn pulley_source_64_bit_calls_are_exact_against_native_source_and_compiled_peers() {
    calls(true);
}

// ---- Hooks and contributions -------------------------------------------------------------

const HOOK_WIT: &str = r#"package example:tally@1.0.0;

/// Waterfall hook over a record of 64-bit integers.
interface tally {
    record entry {
        total: u64,
        delta: s64,
        note: string,
    }
    apply: func(input: entry) -> entry;
}

/// The provider's service, which runs its hook.
interface runner {
    use tally.{entry};
    run: func(input: entry) -> entry;
}

world provider {
    export tally;
    export runner;
}

world extension {
    import tally;
    export tally;
}
"#;

const TALLY: &str = "example:tally/tally@1.0.0";
const RUNNER: &str = "tools::example:tally/runner@1.0.0";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Entry {
    total: u64,
    delta: i64,
    note: String,
}

fn hook_contract(world: &str) -> Contract {
    Contract::from_source("tally.wit", HOOK_WIT, Some(world)).unwrap()
}

fn tally() -> Hook<Entry, Entry> {
    let signature = hook_contract("provider")
        .function_signature(TALLY, "apply", true)
        .unwrap();
    Hook::waterfall(TALLY).signature(signature)
}

/// The SDK-authored source provider `tests/source/test-tally-provider`: its guest runs the hook
/// through the hook's typed WIT function.
fn source_hook_provider(engine: &Engine) -> Orb {
    engine
        .load(&source::fixture("test-tally-provider"), Install::default())
        .unwrap()
}

/// A typed Rust provider running the same hook natively.
fn native_hook_provider() -> Orb {
    let declared = tally();
    let bound = declared.clone();
    let service = hook_contract("provider")
        .plan("tools", 1, &Default::default())
        .unwrap()
        .orb
        .services
        .into_iter()
        .find(|service| service.name == RUNNER)
        .unwrap();
    Orb::new("tools", 1).hook(declared).service(
        Service::new(RUNNER, move |declaration| declaration.hook(&bound))
            .signature(service.signature)
            .method("run", |hook, context, (input,): (Entry,)| async move {
                hook.run(&context, input).await
            }),
    )
}

/// The SDK-authored source contributor `tests/source/test-tally-extension`. It refuses anything
/// but `bigint` integers, so a lossy JavaScript number cannot pass silently.
fn source_contributor(engine: &Engine) -> Orb {
    engine
        .load(
            &source::fixture("test-tally-extension"),
            Install::default().requires(TALLY, "tools", 1),
        )
        .unwrap()
}

/// A typed Rust contributor after the source one.
fn native_contributor() -> Orb {
    Orb::new("native", 1)
        .requires(Requirement {
            signatures: Default::default(),
            hook_signatures: Default::default(),
            orb: "tools".into(),
            api_version: 1,
            methods: Default::default(),
            hooks: [TALLY.into()].into(),
        })
        .contribute("tools", &tally(), 10, |_, input: Entry| async move {
            Ok(Entry {
                total: input.total - 2,
                delta: input.delta + 2,
                note: input.note + "+native",
            })
        })
}

fn hooks(interpreted: bool) {
    let engine = engine(interpreted);
    for source_provider in [false, true] {
        let provider = if source_provider {
            source_hook_provider(&engine)
        } else {
            native_hook_provider()
        };
        let mut pool = LocalPool::new();
        let host = Host::new(pool.spawner());
        host.install(vec![
            provider,
            source_contributor(&engine),
            native_contributor(),
        ])
        .unwrap();
        let runner = reference(&mut pool, &host, RUNNER);
        for (input, expected) in [
            (
                Entry {
                    total: u64::MAX,
                    delta: i64::MIN,
                    note: "x".into(),
                },
                Entry {
                    total: u64::MAX - 3,
                    delta: i64::MIN + 3,
                    note: "x+source+native".into(),
                },
            ),
            (
                Entry {
                    total: 3,
                    delta: i64::MAX - 3,
                    note: "9007199254740993".into(),
                },
                Entry {
                    total: 0,
                    delta: i64::MAX,
                    note: "9007199254740993+source+native".into(),
                },
            ),
        ] {
            let arguments = serde_json::to_value((&input,)).unwrap();
            let result = portable_call(&mut pool, &host, &runner, "run", arguments)
                .unwrap_or_else(|error| panic!("source provider {source_provider}: {error}"));
            assert_eq!(
                serde_json::from_value::<Entry>(result).unwrap(),
                expected,
                "source provider: {source_provider}"
            );
        }
        // The source participant's exact subtraction refuses to go below zero: its `-1n` is not
        // a `u64`, so the canonical lowering of its result refuses it. The hook function has no
        // WIT error lane, so this is a trap of the contributor's revision (declared breaking
        // semantics), never a wrapped value.
        let below = serde_json::to_value((Entry {
            total: 0,
            delta: 0,
            note: String::new(),
        },))
        .unwrap();
        let refused = portable_call(&mut pool, &host, &runner, "run", below)
            .expect_err("a negative u64 result was accepted");
        assert!(
            refused.message.contains("trap"),
            "source provider {source_provider}: {refused}"
        );
        // The quarantined contributor never runs again: a valid input now fails too.
        let valid = serde_json::to_value((Entry {
            total: 3,
            delta: 0,
            note: String::new(),
        },))
        .unwrap();
        let after = portable_call(&mut pool, &host, &runner, "run", valid);
        assert!(
            after.is_err(),
            "source provider {source_provider}: the trapped contributor kept serving: {after:?}"
        );
        let _ = runner.release();
        // Closing may report the quarantined revisions; every Store is physically retired.
        let _ = pool.run_until(host.close());
        assert_eq!(engine.instances(), 0);
        assert_eq!(engine.resources(), 0);
    }
}

#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn source_64_bit_hooks_and_contributions_are_exact() {
    hooks(false);
}

#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn pulley_source_64_bit_hooks_and_contributions_are_exact() {
    hooks(true);
}
