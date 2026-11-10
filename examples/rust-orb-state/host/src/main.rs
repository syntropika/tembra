//! Install the counter, watch its kernel-managed state from the host, and keep it across a reload.
//!
//! The state belongs to the kernel object, not to the component: the host observes it through
//! the reference without calling a method, and a reload replaces the component with a fresh
//! instance while the object and its state stay.
use futures::executor::LocalPool;
use serde_json::{Value, json};
use std::rc::Rc;
use tembra::{Host, Reference};
use tembra_executor_wasm::{Engine, Install, Limits};

/// The service of the Orb `counter`, named after its exported interface.
const COUNTER: &str = "counter::example:counter/counter@1.0.0";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let artifact = std::env::args()
        .nth(1)
        .ok_or("usage: host <path to counter.orb.wasm>")?;
    let bytes = std::fs::read(&artifact)?;
    let engine = Engine::new(Limits::default())?;
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    // The world imports only the kernel's state interface: nothing to require, nothing to grant.
    host.install(vec![engine.load(&bytes, Install::default())?])?;

    // 1. A reference to one counter object; creating it declares its state with the kernel.
    let counter = Rc::new(host.get(COUNTER, vec!["main".into()], "alice")?);
    pool.run_until(counter.ready())?;

    // 2. The host observes the object's state like any native state, with no method involved:
    //    the current value now, and every change the kernel adopts from here on.
    let state = counter.state::<Value>()?;
    let _watch = state.subscribe(|value| {
        println!("observed state      {value}");
        Ok(())
    })?;

    println!(
        "add(5)           -> {}",
        call(&mut pool, &host, &counter, "add", json!([5]))?
    );
    println!(
        "add(-2)          -> {}",
        call(&mut pool, &host, &counter, "add", json!([-2]))?
    );

    // 3. Reload the Orb: a second load of the same artifact is a new revision with a fresh
    //    component instance. The object stays, and so does its state; a count kept in a static
    //    would have started again from zero.
    pool.run_until(host.reload(vec![engine.load(&bytes, Install::default())?], 5_000))?;
    pool.run_until(host.settled());
    println!(
        "reloaded            component instances: {}",
        engine.instances()
    );
    println!(
        "read()           -> {}",
        call(&mut pool, &host, &counter, "read", json!([]))?
    );
    println!(
        "add(1)           -> {}",
        call(&mut pool, &host, &counter, "add", json!([1]))?
    );

    counter.release()?;
    pool.run_until(host.close())?;
    // Cleanup failures (a resource closer, an observer) are reported here, not as call errors.
    let errors = host.errors();
    if !errors.is_empty() {
        return Err(format!("host errors: {errors:?}").into());
    }
    Ok(())
}

/// Call one method of the counter under a root context of the principal `alice`.
fn call(
    pool: &mut LocalPool,
    host: &Host,
    counter: &Rc<Reference>,
    method: &'static str,
    arguments: Value,
) -> tembra::Result<Value> {
    let counter = counter.clone();
    pool.run_until(host.root("alice", move |context| {
        Box::pin(async move { counter.call_portable(&context, method, arguments).await })
    }))
}
