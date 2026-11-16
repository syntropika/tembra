//! The wiring: install the compiled `gate`, which declares the hook, and the compiled `blocker`,
//! which contributes to it, then drive three admissions through the gate. No Orb is defined here.
use futures::executor::LocalPool;
use serde_json::{Value, json};
use std::rc::Rc;
use tembra::wasm::{Engine, Install, Limits};
use tembra::{Host, Reference};

/// The hook identity: the `guard` interface the gate derives from its own code, named by the
/// contributor's `#[hook]` attribute and bound here.
const GUARD: &str = "example:gate/guard@1.0.0";
/// The gate's service, derived from its WIT like any compiled Orb's.
const ADMISSIONS: &str = "gate::example:gate/admissions@1.0.0";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let (Some(gate), Some(blocker)) = (arguments.next(), arguments.next()) else {
        return Err("expected gate.orb.wasm and blocker.orb.wasm".into());
    };

    // 1. The provider. Its artifact declares the hook; the installer requires and grants nothing.
    let engine = Engine::new(Limits::default())?;
    let gate = engine.load(&std::fs::read(gate)?, Install::default())?;
    // 2. The contributor. Its artifact names the contribution; the installation binds the
    //    imported hook interface to the `gate` Orb, like any other required import.
    let blocker = engine.load(
        &std::fs::read(blocker)?,
        Install::default().requires(GUARD, "gate", 1),
    )?;
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![gate, blocker])?;

    // 3. Three admissions: each `admit` runs the hook's plan, and the contributor blocks the
    //    negative value.
    let admissions = Rc::new(host.get(ADMISSIONS, vec!["main".into()], "alice")?);
    pool.run_until(admissions.ready())?;
    for value in [5, -3, 8] {
        let decision = call(&mut pool, &host, &admissions, "admit", json!([value]))?;
        println!("admit({value}) -> {decision}");
    }
    admissions.release()?;
    pool.run_until(host.close())?;
    // Cleanup failures are reported here, not as call errors.
    let errors = host.errors();
    if !errors.is_empty() {
        return Err(format!("host errors: {errors:?}").into());
    }
    Ok(())
}

/// Call one method under a root context of the principal `alice`.
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
