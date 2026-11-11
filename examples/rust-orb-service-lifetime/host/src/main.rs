//! The wiring: install the compiled `ledger`, then for each of its three services add to the
//! balance, release every reference, acquire a new one and add again. What the second addition
//! returns shows whether the object survived the release. No Orb is defined here.
use futures::executor::LocalPool;
use serde_json::{Value, json};
use std::rc::Rc;
use tembra::{Host, Reference};
use tembra_executor_wasm::{Engine, Install, Limits};

/// The three services of the Orb `ledger`, each named after its exported interface, with the
/// principal that may acquire its `main` object: a host-lifetime object belongs to the host.
const SERVICES: [(&str, &str, &str); 3] = [
    ("pinned", "ledger::example:ledger/pinned@1.0.0", "host"),
    ("lingering", "ledger::example:ledger/lingering@1.0.0", "alice"),
    ("passing", "ledger::example:ledger/passing@1.0.0", "alice"),
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let artifact = std::env::args()
        .nth(1)
        .ok_or("usage: host <path to ledger.orb.wasm>")?;
    let engine = Engine::new(Limits::default())?;
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    // The world imports only the kernel's state interface: nothing to require, nothing to grant.
    // Installing creates the pinned service's `main` object at once.
    host.install(vec![engine.load(&std::fs::read(&artifact)?, Install::default())?])?;

    for (label, service, principal) in SERVICES {
        let first = acquire(&mut pool, &host, service, principal)?;
        let after_five = call(&mut pool, &host, &first, principal, "add", json!([5]))?;
        first.release()?;
        // Let the kernel release what the dropped reference no longer keeps alive.
        pool.run_until(host.settled());
        let second = acquire(&mut pool, &host, service, principal)?;
        let after_one = call(&mut pool, &host, &second, principal, "add", json!([1]))?;
        second.release()?;
        println!("{label:<10} add(5) -> {after_five}   released, acquired again   add(1) -> {after_one}");
    }
    pool.run_until(host.close())?;
    // Cleanup failures are reported here, not as call errors.
    let errors = host.errors();
    if !errors.is_empty() {
        return Err(format!("host errors: {errors:?}").into());
    }
    Ok(())
}

/// A reference to the `main` object of `service`, acquired by `principal`, ready to be called.
fn acquire(
    pool: &mut LocalPool,
    host: &Host,
    service: &str,
    principal: &str,
) -> tembra::Result<Rc<Reference>> {
    let reference = Rc::new(host.get(service, vec!["main".into()], principal)?);
    pool.run_until(reference.ready())?;
    Ok(reference)
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
