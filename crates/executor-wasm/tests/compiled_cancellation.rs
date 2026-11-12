//! A compiled Orb keeps running after its call is cancelled, as a source Orb does: the kernel
//! refuses the call's writes, its cancellation and its principal from then on, and the worker
//! answers the guest without a trap wherever a true answer exists. The call's principal is the
//! one it was admitted with, cancelling again changes nothing, and an update answers with the
//! value the kernel retained, the previous one. The revision keeps serving.
//!
//! The fixture is a real component encoded from a small core module, so the test runs without a
//! guest toolchain.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::rc::Rc;
use tembra::{Host, Reference};
use tembra_executor_wasm::{Engine, Install, Limits};

#[path = "support/components.rs"]
mod components;

const WIT: &str = r#"
package example:polite@1.0.0;
interface probe {
  /// Cancel the current call, then read its principal, cancel it again and store the given
  /// JSON as the state; the principal and the value the kernel retained.
  late: func(value: string) -> tuple<string, string>;
  /// The JSON text the kernel retains for this object.
  read: func() -> string;
}
world polite {
  import tembra:guest/context@1.0.0;
  import tembra:guest/state@1.0.0;
  export tembra:guest/state-init@1.0.0;
  export probe;
}
"#;

const PROBE: &str = "polite::example:polite/probe@1.0.0";

/// Return areas from 16 up lie below the bump allocator's first block at 1024; the tuple's two
/// strings are the areas the principal and the update fill, side by side at 16 and 24.
const MODULE: &str = r#"(module
  (import "tembra:guest/context@1.0.0" "principal" (func $principal (param i32)))
  (import "tembra:guest/context@1.0.0" "cancel" (func $cancel))
  (import "tembra:guest/state@1.0.0" "read" (func $read (param i32)))
  (import "tembra:guest/state@1.0.0" "update" (func $update (param i32 i32 i32)))
  (memory (export "memory") 1)
  (global $heap (mut i32) (i32.const 1024))
  ;; The initial value of the state, as the component's own data.
  (data (i32.const 64) "0")
  (func (export "cabi_realloc") (param $old i32) (param $old_size i32) (param $align i32) (param $size i32) (result i32)
    (local $block i32)
    global.get $heap
    local.get $align i32.const 1 i32.sub i32.add
    i32.const 0 local.get $align i32.sub i32.and
    local.tee $block
    local.get $size i32.add
    global.set $heap
    local.get $block)
  ;; declare(service) -> option<declaration>: some, initial "0", version 1, validated.
  (func (export "tembra:guest/state-init@1.0.0#declare") (param i32 i32) (result i32)
    (i32.store (i32.const 128) (i32.const 1))
    (i32.store (i32.const 132) (i32.const 64))
    (i32.store (i32.const 136) (i32.const 1))
    (i32.store (i32.const 140) (i32.const 1))
    (i32.store8 (i32.const 144) (i32.const 1))
    i32.const 128)
  ;; validate(service, value) -> result<_, string>: every value is accepted.
  (func (export "tembra:guest/state-init@1.0.0#validate") (param i32 i32 i32 i32) (result i32)
    (i32.store (i32.const 160) (i32.const 0))
    i32.const 160)
  (func (export "example:polite/probe@1.0.0#late") (param $value i32) (param $len i32) (result i32)
    call $cancel
    i32.const 16 call $principal
    call $cancel
    local.get $value local.get $len i32.const 24 call $update
    i32.const 16)
  (func (export "example:polite/probe@1.0.0#read") (result i32)
    i32.const 40 call $read i32.const 40))"#;

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
fn a_cancelled_call_reads_its_principal_cancels_again_and_keeps_the_state() {
    let artifact = components::encode(WIT, "polite", MODULE, "polite", &Default::default());
    let polite = Engine::new(Limits::default())
        .unwrap()
        .load(&artifact, Install::default())
        .unwrap();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![polite]).unwrap();
    let probe = Rc::new(host.get(PROBE, vec!["main".into()], "alice").unwrap());
    pool.run_until(probe.ready()).unwrap();
    // After its own cancellation the call still learns who called it, and its write is not
    // applied: the update answers with the value the kernel kept.
    assert_eq!(
        call(&mut pool, &host, &probe, "alice", "late", json!(["7"])).unwrap(),
        json!(["alice", "0"])
    );
    // No trap retired the Store: the revision serves the next call, and the state is unchanged.
    assert_eq!(
        call(&mut pool, &host, &probe, "alice", "read", json!([])).unwrap(),
        "0"
    );
    probe.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}
