//! A compiled Orb's kernel-managed state: declared by the component, kept by the kernel across
//! reloads, validated by the new instance before a reload adopts it, and owned by objects whose
//! lifetime the artifact's manifest chooses.
//!
//! The fixture is a real component encoded from a small core module, so the test runs without a
//! guest toolchain. Its state is the JSON text the caller sets; its validator refuses the text `2`.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::rc::Rc;
use tembra::{Host, Reference};
use tembra_contracts::{Lifetime, Manifest, ServicePolicy};
use tembra_executor_wasm::{Engine, Install, Limits};

#[path = "support/components.rs"]
mod components;

const WIT: &str = r#"
package example:tally@1.0.0;
interface counter {
  /// Store the JSON text as the state of this object.
  set: func(value: string);
  /// The JSON text the kernel retains for this object.
  read: func() -> string;
}
world tally {
  import tembra:guest/state@1.0.0;
  export tembra:guest/state-init@1.0.0;
  export counter;
}
"#;

const COUNTER: &str = "tally::example:tally/counter@1.0.0";

/// Return areas from 128 up lie below the bump allocator's first block at 1024.
const MODULE: &str = r#"(module
  (import "tembra:guest/state@1.0.0" "read" (func $read (param i32)))
  (import "tembra:guest/state@1.0.0" "update" (func $update (param i32 i32 i32)))
  (memory (export "memory") 1)
  (global $heap (mut i32) (i32.const 1024))
  ;; The initial value and the refusal, as the component's own data.
  (data (i32.const 64) "0")
  (data (i32.const 80) "two is refused")
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
  ;; validate(service, value) -> result<_, string>: the one-byte text "2" is refused.
  (func (export "tembra:guest/state-init@1.0.0#validate") (param $service i32) (param $service_len i32) (param $value i32) (param $len i32) (result i32)
    (local $refused i32)
    local.get $len i32.const 1 i32.eq
    local.get $value i32.load8_u i32.const 50 i32.eq
    i32.and
    local.set $refused
    (i32.store (i32.const 160) (local.get $refused))
    (i32.store (i32.const 164) (i32.const 80))
    (i32.store (i32.const 168) (i32.const 14))
    i32.const 160)
  ;; set(value): the caller's text becomes the state; the kernel's answer is left at 192.
  (func (export "example:tally/counter@1.0.0#set") (param i32 i32)
    local.get 0 local.get 1 i32.const 192 call $update)
  ;; read() -> string: the area the import fills is the one returned.
  (func (export "example:tally/counter@1.0.0#read") (result i32)
    i32.const 208 call $read i32.const 208))"#;

fn artifact(manifest: &Manifest) -> Vec<u8> {
    components::encode(WIT, "tally", MODULE, "tally", manifest)
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

fn reference(pool: &mut LocalPool, host: &Host, principal: &str) -> Rc<Reference> {
    let reference = Rc::new(host.get(COUNTER, vec!["main".into()], principal).unwrap());
    pool.run_until(reference.ready()).unwrap();
    reference
}

/// The kernel keeps the state across a reload once the new instance validated it; a value the
/// new instance refuses keeps the running revision and its state.
#[test]
fn a_reload_adopts_validated_state_and_refuses_the_rest() {
    let engine = Engine::new(Limits::default()).unwrap();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let manifest = Manifest::default();
    host.install(vec![
        engine
            .load(&artifact(&manifest), Install::default())
            .unwrap(),
    ])
    .unwrap();
    let counter = reference(&mut pool, &host, "alice");
    assert_eq!(
        call(&mut pool, &host, &counter, "alice", "read", json!([])).unwrap(),
        "0"
    );
    call(&mut pool, &host, &counter, "alice", "set", json!(["1"])).unwrap();
    assert_eq!(
        call(&mut pool, &host, &counter, "alice", "read", json!([])).unwrap(),
        "1"
    );
    // A fresh component instance inherits the object and its validated state.
    pool.run_until(host.reload(
        vec![
            engine
                .load(&artifact(&manifest), Install::default())
                .unwrap(),
        ],
        5_000,
    ))
    .unwrap();
    pool.run_until(host.settled());
    assert_eq!(engine.instances(), 1);
    assert_eq!(
        call(&mut pool, &host, &counter, "alice", "read", json!([])).unwrap(),
        "1"
    );
    // The validator of the next instance refuses `2`: the reload fails, the state stays served.
    call(&mut pool, &host, &counter, "alice", "set", json!(["2"])).unwrap();
    let refused = pool
        .run_until(host.reload(
            vec![
                engine
                    .load(&artifact(&manifest), Install::default())
                    .unwrap(),
            ],
            5_000,
        ))
        .expect_err("the reload is refused");
    assert!(refused.to_string().contains("two is refused"), "{refused}");
    pool.run_until(host.settled());
    assert_eq!(
        call(&mut pool, &host, &counter, "alice", "read", json!([])).unwrap(),
        "2"
    );
    counter.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}

/// A host-lifetime service, chosen in the manifest, keeps its object and the object's state
/// after every reference is released; a referenced one starts a fresh object.
#[test]
fn pinned_objects_keep_their_state_between_references() {
    for (policy, principal, kept) in [
        (ServicePolicy::default(), "alice", "0"),
        (
            ServicePolicy {
                lifetime: Lifetime::Host,
                linger_ms: 0,
            },
            "host",
            "1",
        ),
    ] {
        let engine = Engine::new(Limits::default()).unwrap();
        let mut pool = LocalPool::new();
        let host = Host::new(pool.spawner());
        let manifest = Manifest {
            services: [("example:tally/counter@1.0.0".to_owned(), policy)].into(),
            ..Default::default()
        };
        host.install(vec![
            engine
                .load(&artifact(&manifest), Install::default())
                .unwrap(),
        ])
        .unwrap();
        let counter = reference(&mut pool, &host, principal);
        call(&mut pool, &host, &counter, principal, "set", json!(["1"])).unwrap();
        counter.release().unwrap();
        pool.run_until(host.settled());
        let counter = reference(&mut pool, &host, principal);
        assert_eq!(
            call(&mut pool, &host, &counter, principal, "read", json!([])).unwrap(),
            kept,
            "{policy:?}"
        );
        counter.release().unwrap();
        pool.run_until(host.close()).unwrap();
        assert!(host.errors().is_empty(), "{:?}", host.errors());
    }
}
