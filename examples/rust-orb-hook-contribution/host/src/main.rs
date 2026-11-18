//! The wiring: install the native `gate` (its own crate) and the compiled `filter` built with
//! `cargo tembra`, drive three admissions, reload the contributor and drive one more. No Orb is
//! defined here.
use futures::executor::LocalPool;
use serde_json::{Value, json};
use std::rc::Rc;
use tembra::wasm::{Engine, Install, Limits};
use tembra::{Host, Reference};
use tembra_example_native_gate::GUARD;

/// The contributor's own service, derived from its WIT like any compiled Orb's.
const REPORT: &str = "filter::example:filter/report@1.0.0";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let artifact = std::env::args_os()
        .nth(1)
        .ok_or("expected filter.orb.wasm")?;

    // 1. The compiled contributor. Its artifact names the contribution; the installation binds
    //    the imported hook interface to the `gate` Orb, like any other required import.
    let engine = Engine::new(Limits::default())?;
    let bytes = std::fs::read(artifact)?;
    let filter = engine.load(&bytes, Install::default().requires(GUARD, "gate", 1))?;
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![tembra_example_native_gate::orb()?, filter])?;

    // 2. Three admissions: the contributor sees each one and blocks the negative value.
    let gate = Rc::new(host.get("gate", vec!["main".into()], "alice")?);
    let report = Rc::new(host.get(REPORT, vec!["main".into()], "alice")?);
    pool.run_until(gate.ready())?;
    pool.run_until(report.ready())?;
    for value in [5, -3, 8] {
        let decision = call(&mut pool, &host, &gate, "admit", json!(value))?;
        println!("admit({value}) -> {decision}");
    }
    println!(
        "gate ledger -> {}",
        call(&mut pool, &host, &gate, "ledger", Value::Null)?
    );
    println!(
        "filter report -> {}",
        call(&mut pool, &host, &report, "read", json!([]))?
    );

    // 3. Reload the contributor: a second load of the same artifact is a new revision with a
    //    fresh component instance. The kernel reopens the root's resource in the new heap, so the
    //    counts start again; the ledger of the native `gate` is untouched.
    let replacement = engine.load(&bytes, Install::default().requires(GUARD, "gate", 1))?;
    pool.run_until(host.reload(vec![replacement], 5_000))?;
    pool.run_until(host.settled());
    println!("filter reloaded");
    println!(
        "admit(-1) -> {}",
        call(&mut pool, &host, &gate, "admit", json!(-1))?
    );
    println!(
        "gate ledger -> {}",
        call(&mut pool, &host, &gate, "ledger", Value::Null)?
    );
    println!(
        "filter report -> {}",
        call(&mut pool, &host, &report, "read", json!([]))?
    );
    gate.release()?;
    report.release()?;
    pool.run_until(host.close())?;
    // Cleanup failures (a resource closer, an observer) are reported here, not as call errors.
    let errors = host.errors();
    if !errors.is_empty() {
        return Err(format!("host errors: {errors:?}").into());
    }
    Ok(())
}

/// Call one method under a root context of the principal `alice`. The portable entrypoint takes
/// JSON for native and compiled services alike.
fn call(
    pool: &mut LocalPool,
    host: &Host,
    reference: &Rc<Reference>,
    method: &'static str,
    arguments: Value,
) -> tembra::Result<Value> {
    let reference = reference.clone();
    pool.run_until(host.root("alice", move |context| {
        Box::pin(async move { reference.call_portable(&context, method, arguments).await })
    }))
}
