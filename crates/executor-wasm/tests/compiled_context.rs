//! A compiled Orb reads the kernel's view of its call through `tembra:guest/context`: the call's
//! principal and cancellation, the address of the object it runs on, and the host's log.
//!
//! The fixture is a real component encoded from a small core module, so the test runs without a
//! guest toolchain. Each export forwards one import; the return area an import fills is the one
//! the export returns, because a string, a record of a list and a string, and a tuple of the
//! same two have one canonical layout.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::{
    rc::Rc,
    sync::{Arc, Mutex},
};
use tembra::{Host, Reference};
use tembra_executor_wasm::{Contract, Engine, Install, Limits, LogRecord};

/// The kernel's package, bundled with the contracts; the fixture's world imports its context.
const GUEST: &str = include_str!("../../contracts/wit/tembra/guest.wit");

const WIT: &str = r#"
package example:aware@1.0.0;
interface probe {
  /// The principal of the current call.
  whoami: func() -> string;
  /// The key and the principal of the object running the call.
  whereami: func() -> tuple<list<string>, string>;
  cancelled: func() -> bool;
  /// Cancel the current call, then ask whether it is cancelled.
  abort: func() -> bool;
  /// Log the message at level info.
  note: func(message: string);
}
world aware { import tembra:guest/context@1.0.0; export probe; }
"#;

const PROBE: &str = "aware::example:aware/probe@1.0.0";

/// Return areas at 16 and 32 lie below the bump allocator's first block at 1024.
const MODULE: &str = r#"(module
  (import "tembra:guest/context@1.0.0" "object" (func $object (param i32)))
  (import "tembra:guest/context@1.0.0" "principal" (func $principal (param i32)))
  (import "tembra:guest/context@1.0.0" "cancelled" (func $cancelled (result i32)))
  (import "tembra:guest/context@1.0.0" "cancel" (func $cancel))
  (import "tembra:guest/context@1.0.0" "log" (func $log (param i32 i32 i32)))
  (memory (export "memory") 1)
  (global $heap (mut i32) (i32.const 1024))
  ;; A bump allocator: the host lifts strings and lists into fresh aligned blocks; nothing is freed.
  (func (export "cabi_realloc") (param $old i32) (param $old_size i32) (param $align i32) (param $size i32) (result i32)
    (local $block i32)
    global.get $heap
    local.get $align i32.const 1 i32.sub i32.add
    i32.const 0 local.get $align i32.sub i32.and
    local.tee $block
    local.get $size i32.add
    global.set $heap
    local.get $block)
  (func (export "example:aware/probe@1.0.0#whoami") (result i32)
    i32.const 16 call $principal i32.const 16)
  (func (export "example:aware/probe@1.0.0#whereami") (result i32)
    i32.const 32 call $object i32.const 32)
  (func (export "example:aware/probe@1.0.0#cancelled") (result i32)
    call $cancelled)
  (func (export "example:aware/probe@1.0.0#abort") (result i32)
    call $cancel call $cancelled)
  ;; Level 2 is `info`, the third case of the enum.
  (func (export "example:aware/probe@1.0.0#note") (param i32 i32)
    i32.const 2 local.get 0 local.get 1 call $log))"#;

fn artifact() -> Vec<u8> {
    let mut resolve = wit_parser::Resolve::default();
    resolve.push_str("guest.wit", GUEST).unwrap();
    let package = resolve.push_str("aware.wit", WIT).unwrap();
    let world = resolve.select_world(&[package], Some("aware")).unwrap();
    let mut core = wat::parse_str(MODULE).unwrap();
    wit_component::embed_component_metadata(
        &mut core,
        &resolve,
        world,
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
    Contract::from_source("aware.wit", WIT, Some("aware"))
        .unwrap()
        .component_artifact(&component, "aware", 1, &Default::default())
        .unwrap()
}

/// Call one method under a root of `principal`.
fn call(
    pool: &mut LocalPool,
    host: &Host,
    reference: &Rc<Reference>,
    principal: &str,
    method: &'static str,
    arguments: Value,
) -> tembra::Result<Value> {
    let reference = reference.clone();
    pool.run_until(host.root(principal, move |context| {
        async move {
            reference
                .call::<_, Value>(&context, method, arguments)
                .await
        }
        .boxed_local()
    }))
}

#[test]
fn a_compiled_orb_reads_its_call_and_its_object_and_logs_through_the_kernel() {
    let records: Arc<Mutex<Vec<LogRecord>>> = Arc::default();
    let sink = records.clone();
    let engine = Engine::new(Limits::default())
        .unwrap()
        .on_log(move |record| sink.lock().unwrap().push(record.clone()));
    let aware = engine.load(&artifact(), Install::default()).unwrap();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![aware]).unwrap();
    let probe = Rc::new(
        host.get(PROBE, vec!["room".into(), "7".into()], "alice")
            .unwrap(),
    );
    pool.run_until(probe.ready()).unwrap();
    // The call's principal is the root's; the object's address is the reference's key and the
    // principal that acquired it.
    assert_eq!(
        call(&mut pool, &host, &probe, "alice", "whoami", json!([])).unwrap(),
        "alice"
    );
    assert_eq!(
        call(&mut pool, &host, &probe, "alice", "whereami", json!([])).unwrap(),
        json!([["room", "7"], "alice"])
    );
    assert_eq!(
        call(&mut pool, &host, &probe, "alice", "cancelled", json!([])).unwrap(),
        false
    );
    // A record carries the host's attribution, never a guest-chosen Orb name.
    assert_eq!(
        call(&mut pool, &host, &probe, "alice", "note", json!(["hello"])).unwrap(),
        Value::Null
    );
    {
        let records = records.lock().unwrap();
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(
            (
                records[0].orb.as_str(),
                records[0].level.as_str(),
                records[0].message.as_str()
            ),
            ("aware", "info", "hello")
        );
    }
    // The guest cancels its own call and observes the cancellation at once.
    let aborted = call(&mut pool, &host, &probe, "alice", "abort", json!([]));
    assert_eq!(aborted.unwrap(), true);
    probe.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}
