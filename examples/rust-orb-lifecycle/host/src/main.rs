//! Package the hand-written Rust counter exactly like a componentized JavaScript Orb, install it,
//! watch its kernel state, see an update refused by the state's rule, and keep the state across a
//! reload whose new instance validates it.
use futures::executor::LocalPool;
use serde_json::{Value, json};
use std::rc::Rc;
use tembra::{Host, Reference};
use tembra_executor_wasm::{Contract, Engine, Install, Limits};

/// The service of the Orb `counter`, named after its exported interface.
const COUNTER: &str = "counter::example:counter/counter@1.0.0";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let usage = "usage: host <counter component .wasm> <contract directory>";
    let component = std::fs::read(args.next().ok_or(usage)?)?;
    let contract = args.next().ok_or(usage)?;

    // 1. Package: the public contract checks the component (its lifecycle export, its host
    //    imports and its public world) and marks the artifact as a `tembra:orb` component.
    let contract = Contract::from_dir(&contract, Some("tally"))?;
    let artifact = contract.source_component_artifact(&component, "counter", 1)?;
    println!("packaged            {} bytes", artifact.len());

    // `host.log` from the component reaches the engine's log sink, attributed to the Orb.
    let engine = Engine::new(Limits::default())?.on_log(|record| {
        println!("  log {} {}: {}", record.orb, record.level, record.message);
    });
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![engine.load(&artifact, Install::default())?])?;

    // 2. A reference to one counter object: the host calls `lifecycle.create` for the Orb root
    //    and then for this object, which declares its state.
    let counter = Rc::new(host.get(COUNTER, vec!["main".into()], "alice")?);
    pool.run_until(counter.ready())?;

    // The host observes the object's state like any native state, with no method involved.
    let state = counter.state::<Value>()?;
    let _watch = state.subscribe(|value| {
        println!("observed state      {value}");
        Ok(())
    })?;

    let five_billion = json!([5_000_000_000_i64]);
    println!(
        "add(5000000000)  -> {}",
        outcome(call(&mut pool, &host, &counter, "add", five_billion)?)
    );
    println!(
        "add(-2)          -> {}",
        outcome(call(&mut pool, &host, &counter, "add", json!([-2]))?)
    );
    // The rule refuses a negative total: `add` returns the refusal and updates nothing.
    let six_billion_down = json!([-6_000_000_000_i64]);
    println!(
        "add(-6000000000) -> {}",
        outcome(call(&mut pool, &host, &counter, "add", six_billion_down)?)
    );

    // 3. Reload: a new revision with a fresh component instance. The object and its state stay;
    //    the kernel first asks the new instance's `lifecycle.validate` about the retained state.
    pool.run_until(host.reload(vec![engine.load(&artifact, Install::default())?], 5_000))?;
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
    let errors = host.errors();
    if !errors.is_empty() {
        return Err(format!("host errors: {errors:?}").into());
    }
    Ok(())
}

/// Print a WIT `result<count, string>` as the boundary carries it.
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
