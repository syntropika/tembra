//! The wiring: install the compiled `audit`, observe its log, and call it through two references
//! acquired by different principals with different keys. No Orb is defined here.
use futures::executor::LocalPool;
use serde_json::{Value, json};
use std::rc::Rc;
use tembra::wasm::{Engine, Install, Limits};
use tembra::{Host, Reference};

/// The audit's service, derived from its WIT like any compiled Orb's.
const AUDITOR: &str = "audit::example:audit/auditor@1.0.0";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let artifact = std::env::args()
        .nth(1)
        .ok_or("usage: host <path to audit.orb.wasm>")?;
    // 1. The engine receives every log record a compiled Orb writes, attributed by the host.
    let engine = Engine::new(Limits::default())?.on_log(|record| {
        println!(
            "log            [{}] {}: {}",
            record.level, record.orb, record.message
        );
    });
    // The world imports only the kernel's context interface: nothing to require, nothing to grant.
    let audit = engine.load(&std::fs::read(&artifact)?, Install::default())?;
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![audit])?;

    // 2. Two objects of the same service: one acquired by alice under the key ["room", "7"],
    //    one by bob under ["desk"]. Each call reports the principal of its root and the address
    //    of the object it ran on.
    let room = Rc::new(host.get(AUDITOR, vec!["room".into(), "7".into()], "alice")?);
    let desk = Rc::new(host.get(AUDITOR, vec!["desk".into()], "bob")?);
    pool.run_until(room.ready())?;
    pool.run_until(desk.ready())?;
    println!(
        "alice, room 7  whoami() -> {}",
        call(&mut pool, &host, &room, "alice", "whoami", json!([]))?
    );
    println!(
        "bob, desk      whoami() -> {}",
        call(&mut pool, &host, &desk, "bob", "whoami", json!([]))?
    );
    // 3. A log record from inside a method, and a call that cancels itself. Methods are called
    //    by their WIT names: `give_up` in Rust is `give-up` in the contract.
    call(
        &mut pool,
        &host,
        &desk,
        "bob",
        "warn",
        json!(["the desk is full"]),
    )?;
    println!(
        "bob, desk      give-up() -> {}",
        call(&mut pool, &host, &desk, "bob", "give-up", json!([]))?
    );
    room.release()?;
    desk.release()?;
    pool.run_until(host.close())?;
    // Cleanup failures are reported here, not as call errors.
    let errors = host.errors();
    if !errors.is_empty() {
        return Err(format!("host errors: {errors:?}").into());
    }
    Ok(())
}

/// Call one method under a root context of `principal`.
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
        Box::pin(async move { reference.call_portable(&context, method, arguments).await })
    }))
}
