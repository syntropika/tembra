//! Consume a prepared Orb through Tembra without configuring Wasmtime or WIT bindings directly.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::{collections::BTreeMap, rc::Rc};
use tembra::Host;
use tembra_executor_wasm::{Binding, Contract, Engine, Grants, Limits, PreparedSource};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let engine_file = arguments
        .next()
        .ok_or("expected engine.wasm and prepared.json")?;
    let orb_file = arguments.next().ok_or("expected prepared.json")?;
    let provider_file = arguments.next().ok_or("expected arithmetic.wasm")?;
    let prepared: PreparedSource = serde_json::from_slice(&std::fs::read(orb_file)?)?;
    let engine = Engine::new(&std::fs::read(engine_file)?, Limits::default())?.on_log(|record| {
        eprintln!("{} [{}] {}", record.orb, record.level, record.message);
    });
    let bindings = BTreeMap::from([(
        "example:integrated/arithmetic@1.0.0".into(),
        Binding::Orb {
            name: "math".into(),
            api_version: 1,
            world: "arithmetic-provider".into(),
        },
    )]);
    let calculator =
        engine.prepare_source("calculator", 1, &prepared, &bindings, &Grants::default())?;
    let provider = Contract::from_bytes(&prepared.contract, Some("arithmetic-provider"))?;
    let arithmetic = engine.prepare_component(
        &provider,
        "math",
        1,
        &std::fs::read(provider_file)?,
        &BTreeMap::new(),
        &Grants::default(),
    )?;
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
