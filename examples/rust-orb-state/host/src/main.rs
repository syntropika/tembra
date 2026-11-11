//! Install the counter, watch its kernel-managed state from the host, see an update refused by
//! the state's own rule, and keep the state across a reload.
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

    // Five billion does not fit in 32 bits: the boundary carries 64-bit integers exactly.
    let five_billion = json!([5_000_000_000_i64]);
    println!(
        "add(5000000000)  -> {}",
        outcome(call(&mut pool, &host, &counter, "add", five_billion)?)
    );
    println!(
        "add(-2)          -> {}",
        outcome(call(&mut pool, &host, &counter, "add", json!([-2]))?)
    );
    // The state's validator refuses a negative total: `add` reports the refusal as its result,
    // the kernel keeps the previous count and the observer sees no change.
    let six_billion_down = json!([-6_000_000_000_i64]);
    println!(
        "add(-6000000000) -> {}",
        outcome(call(&mut pool, &host, &counter, "add", six_billion_down)?)
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
        outcome(call(&mut pool, &host, &counter, "add", json!([1]))?)
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

/// Print a WIT `result<count, string>` as the boundary carries it: `{"tag":"ok","val":...}` or
/// `{"tag":"err","val":"..."}`.
fn outcome(result: Value) -> String {
    match result["tag"].as_str() {
        Some("ok") => format!("ok {}", result["val"]),
        Some("err") => format!("refused: {}", result["val"].as_str().unwrap_or_default()),
        _ => result.to_string(),
    }
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
