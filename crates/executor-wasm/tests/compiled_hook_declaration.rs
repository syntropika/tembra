//! A compiled Orb declares a hook: its artifact's manifest names the composition, the kernel
//! binds the hook while the Orb root is created, and a call of the hook's import runs the plan of
//! contributions, here a native one, exactly as a source provider's `hook.run` does.
//!
//! The fixture is a real component encoded from a small core module: the export of the declared
//! hook traps, because contributions answer it and the provider never does, and the service's
//! method hands its input to the import.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::{cell::RefCell, rc::Rc};
use tembra::{Hook, Host, Orb, Reference, core::Requirement};
use tembra_contracts::{HookComposition, Manifest};
use tembra_executor_wasm::{Contract, Engine, Install, Limits};

#[path = "support/components.rs"]
mod components;

const GUARD: &str = "example:gated/guard@1.0.0";
const GATE: &str = "gate::example:gated/admissions@1.0.0";

const WIT: &str = r#"
package example:gated@1.0.0;
/// The hook: a waterfall over a number, declared by the gate.
interface guard { apply: func(input: u32) -> u32; }
/// The gate's service, which runs its hook over each value.
interface admissions { admit: func(value: u32) -> u32; }
world gate { import guard; export guard; export admissions; }
"#;

const MODULE: &str = r#"(module
  (import "example:gated/guard@1.0.0" "apply" (func $run (param i32) (result i32)))
  (func (export "example:gated/guard@1.0.0#apply") (param i32) (result i32) unreachable)
  (func (export "example:gated/admissions@1.0.0#admit") (param i32) (result i32)
    local.get 0 call $run))"#;

fn provider() -> Vec<u8> {
    components::encode(
        WIT,
        "gate",
        MODULE,
        "gate",
        &Manifest {
            hooks: [(GUARD.to_owned(), HookComposition::Waterfall)].into(),
            ..Default::default()
        },
    )
}

/// A native contributor to the compiled Orb's hook: it records its input and adds one.
fn contributor(seen: Rc<RefCell<Vec<u32>>>) -> Orb {
    let contract = Contract::from_source("gated.wit", WIT, Some("gate")).unwrap();
    let hook = Hook::<u32, u32>::waterfall(GUARD)
        .signature(contract.function_signature(GUARD, "apply", true).unwrap());
    Orb::new("filter", 1)
        .requires(Requirement {
            orb: "gate".into(),
            api_version: 1,
            methods: Default::default(),
            hooks: [GUARD.into()].into(),
            signatures: Default::default(),
            hook_signatures: Default::default(),
        })
        .contribute("gate", &hook, -7, move |_, input: u32| {
            seen.borrow_mut().push(input);
            async move { Ok(input + 1) }
        })
}

fn admit(pool: &mut LocalPool, host: &Host, gate: &Rc<Reference>, value: u32) -> Value {
    let gate = gate.clone();
    pool.run_until(host.root("alice", move |context| {
        async move {
            gate.call::<_, Value>(&context, "admit", json!([value]))
                .await
        }
        .boxed_local()
    }))
    .unwrap()
}

fn gate(pool: &mut LocalPool, host: &Host) -> Rc<Reference> {
    let gate = Rc::new(host.get(GATE, vec!["main".into()], "alice").unwrap());
    pool.run_until(gate.ready()).unwrap();
    gate
}

/// The declared hook is installed from the manifest, never offered as a service, and each call
/// of the import runs the native contribution under the gate's own call.
#[test]
fn a_compiled_provider_runs_its_declared_hook_over_native_contributions() {
    let engine = Engine::new(Limits::default()).unwrap();
    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        engine.load(&provider(), Install::default()).unwrap(),
        contributor(seen.clone()),
    ])
    .unwrap();
    let declared = host
        .kernel()
        .installed_orbs()
        .into_iter()
        .find(|orb| orb.declaration.name == "gate")
        .expect("the gate is installed")
        .declaration;
    assert!(declared.hooks.contains(GUARD), "{:?}", declared.hooks);
    assert_eq!(
        declared
            .services
            .iter()
            .map(|service| service.name.as_str())
            .collect::<Vec<_>>(),
        [GATE]
    );
    let gate = gate(&mut pool, &host);
    assert_eq!(admit(&mut pool, &host, &gate, 5), 6);
    assert_eq!(admit(&mut pool, &host, &gate, 41), 42);
    assert_eq!(*seen.borrow(), [5, 41]);
    gate.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}

/// Without contributions a waterfall hands its input back; the provider's trapping export is
/// never called.
#[test]
fn a_declared_hook_without_contributions_returns_its_input() {
    let engine = Engine::new(Limits::default()).unwrap();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![engine.load(&provider(), Install::default()).unwrap()])
        .unwrap();
    let gate = gate(&mut pool, &host);
    assert_eq!(admit(&mut pool, &host, &gate, 5), 5);
    gate.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}
