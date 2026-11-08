//! Consume a built Orb artifact through Tembra without configuring Wasmtime or WIT bindings directly.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::rc::Rc;
use tembra::Host;
use tembra_executor_wasm::{Engine, Install, Limits};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let engine_file = arguments
        .next()
        .ok_or("expected interpreter.wasm, calculator.wasm and arithmetic.wasm")?;
    let orb_file = arguments.next().ok_or("expected calculator.wasm")?;
    let provider_file = arguments.next().ok_or("expected arithmetic.wasm")?;
    let engine = Engine::new(Limits::default())?
        .interpreter(&std::fs::read(engine_file)?)?
        .on_log(|record| {
            eprintln!("{} [{}] {}", record.orb, record.level, record.message);
        });
    let calculator = engine.load(
        &std::fs::read(orb_file)?,
        Install::default().requires("example:integrated/arithmetic@1.0.0", "arithmetic", 1),
    )?;
    let arithmetic = engine.load(&std::fs::read(provider_file)?, Install::default())?;
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![arithmetic, calculator])?;
    let reference = Rc::new(host.get(
        "calculator::example:integrated/calculation@1.0.0",
        vec!["main".into()],
        "example",
    )?);
    pool.run_until(reference.ready())?;
    let called = reference.clone();
    let result: Value = pool.run_until(host.root("example", move |context| {
        async move {
            called
                .call(&context, "sum", json!([{"values":[3,4,5]}]))
                .await
        }
        .boxed_local()
    }))?;
    println!("{result}");
    reference.release()?;
    pool.run_until(host.close())?;
    assert_eq!(engine.instances(), 0);
    Ok(())
}
