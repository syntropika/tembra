//! A native Orb declares a hook and keeps state; a compiled Orb built with `cargo tembra`
//! contributes to that hook. The host installs both and drives three admissions.
//!
//! Compiled Orbs cannot declare hooks yet, so the provider is native: trusted application code
//! declared with the `tembra` facade. The contributor is the sandboxed one.
use futures::executor::LocalPool;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{cell::RefCell, rc::Rc};
use tembra::{Hook, Host, Orb, Reference, Service};
use tembra_executor_wasm::{Contract, Engine, Install, Limits};

/// The hook identity: the provider's WIT interface, named by the contributor's attribute too.
const GUARD: &str = "example:admission/guard@1.0.0";
/// The contributor's own service, derived from its WIT like any compiled Orb's.
const REPORT: &str = "filter::example:filter/report@1.0.0";

/// The hook payload: the WIT record `admission`.
#[derive(Clone, Serialize, Deserialize)]
struct Admission {
    value: i32,
    blocked: bool,
}

/// The provider's state: what passed, and how many admissions were blocked.
#[derive(Clone, Default, Serialize)]
struct Ledger {
    admitted: Vec<i32>,
    blocked: u32,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let artifact = std::env::args_os()
        .nth(1)
        .ok_or("expected filter.orb.wasm")?;

    // 1. The native provider. The hook carries the WIT callback signature, so a compiled
    //    contributor is checked against the same contract when it is installed.
    let contract = Contract::from_dir(
        concat!(env!("CARGO_MANIFEST_DIR"), "/../wit"),
        Some("provider"),
    )?;
    let guard = Hook::<Admission, Admission>::waterfall_until(GUARD, |input| input.blocked)
        .signature(contract.function_signature(GUARD, "apply", true)?);
    let bound = guard.clone();
    let ledger = Rc::new(RefCell::new(Ledger::default()));
    let gate = Orb::new("gate", 1).hook(guard).service(
        Service::new("gate", move |declaration| {
            Ok((declaration.hook(&bound)?, ledger.clone()))
        })
        .method("admit", |gate, context, value: i32| async move {
            let input = Admission {
                value,
                blocked: false,
            };
            let decision = gate.0.run(&context, input).await?;
            let mut ledger = gate.1.borrow_mut();
            if decision.blocked {
                ledger.blocked += 1;
            } else {
                ledger.admitted.push(decision.value);
            }
            Ok(decision)
        })
        .method("ledger", |gate, _, (): ()| async move {
            Ok(gate.1.borrow().clone())
        }),
    );

    // 2. The compiled contributor. Its artifact names the contribution; the installation binds
    //    the imported hook interface to the `gate` Orb, like any other required import.
    let engine = Engine::new(Limits::default())?;
    let filter = engine.load(
        &std::fs::read(artifact)?,
        Install::default().requires(GUARD, "gate", 1),
    )?;
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![gate, filter])?;

    // 3. Three admissions: the contributor sees each one and blocks the negative value.
    let gate = Rc::new(host.get("gate", vec!["main".into()], "alice")?);
    let report = Rc::new(host.get(REPORT, vec!["main".into()], "alice")?);
    pool.run_until(gate.ready())?;
    pool.run_until(report.ready())?;
    let mut call = |reference: &Rc<Reference>, method: &'static str, arguments: Value| {
        let reference = reference.clone();
        pool.run_until(host.root("alice", move |context| {
            // The portable entrypoint takes JSON for native and compiled services alike.
            Box::pin(async move { reference.call_value(&context, method, arguments).await })
        }))
    };
    for value in [5, -3, 8] {
        println!("admit({value}) -> {}", call(&gate, "admit", json!(value))?);
    }
    println!("gate ledger -> {}", call(&gate, "ledger", Value::Null)?);
    println!("filter report -> {}", call(&report, "read", json!([]))?);
    gate.release()?;
    report.release()?;
    pool.run_until(host.close())?;
    Ok(())
}
