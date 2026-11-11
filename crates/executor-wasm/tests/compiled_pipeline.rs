//! A real compiled Rust Orb imports two interpreted TypeScript Orbs with delayed results.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::{path::PathBuf, rc::Rc, time::Duration};
use tembra::{Host, Orb};
use tembra_contracts::Artifact;
use tembra_executor_wasm::{Contract, Engine, Install, Limits};

const PIPELINE: &str = "example:pipeline/pipeline@1.0.0";

/// Resolve one world of the pipeline test contract.
fn contract(world: &str) -> Contract {
    Contract::from_dir(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/wit/pipeline"),
        Some(world),
    )
    .unwrap()
}

/// Package a closed source bundle whose service waits before doubling.
fn source(engine: &Engine, world: &str, name: &str, service: &str) -> tembra::Result<Orb> {
    let bundle = format!(
        "globalThis.__tembraOrb={{services:{{{service}:{{methods:{{async double(_e,_c,value){{await tembra.wait(40);return value*2;}}}}}}}}}};"
    );
    let bytes = Artifact::package_source(name, 1, contract(world).prepared(), &bundle).unwrap();
    engine.load(&bytes, Install::default())
}

/// Delayed source results reach the compiled guest through sync and P3 async imports.
#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE and TEMBRA_WASM_CONSUMER"]
fn compiled_rust_orb_awaits_delayed_typescript_dependencies() {
    // The acceptance runner does not build the consumer yet; report the skip explicitly.
    let Some(component) = std::env::var_os("TEMBRA_WASM_CONSUMER") else {
        eprintln!("skipped: TEMBRA_WASM_CONSUMER names no compiled pipeline consumer");
        return;
    };
    let interpreter =
        std::fs::read(std::env::var_os("TEMBRA_WASM_ENGINE").expect("TEMBRA_WASM_ENGINE")).unwrap();
    let component = std::fs::read(component).unwrap();
    let engine = Engine::new(Limits::default())
        .unwrap()
        .interpreter(&interpreter)
        .unwrap();
    let arithmetic = source(&engine, "arithmetic-provider", "arithmetic", "arithmetic").unwrap();
    let delayed = source(&engine, "delayed-provider", "delayed", "delayed").unwrap();
    let consumer = engine
        .load(
            &contract("consumer")
                .component_artifact(&component, "pipeline", 1, &Default::default())
                .unwrap(),
            Install::default()
                .requires("example:pipeline/arithmetic@1.0.0", "arithmetic", 1)
                .requires("example:pipeline/delayed@1.0.0", "delayed", 1),
        )
        .unwrap();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![arithmetic, delayed, consumer]).unwrap();
    let reference = Rc::new(
        host.get(
            &format!("pipeline::{PIPELINE}"),
            vec!["main".into()],
            "alice",
        )
        .unwrap(),
    );
    pool.run_until(reference.ready()).unwrap();
    assert_eq!(engine.instances(), 3);
    let called = reference.clone();
    let started = std::time::Instant::now();
    pool.run_until(host.root("alice", move |context| {
        async move {
            assert_eq!(
                called
                    .call::<_, Value>(&context, "quadruple", json!([6]))
                    .await?,
                24
            );
            assert_eq!(
                called
                    .call::<_, Value>(&context, "octuple", json!([3]))
                    .await?,
                24
            );
            Ok(())
        }
        .boxed_local()
    }))
    .unwrap();
    // Five sequential dependency calls each waited inside their source provider.
    assert!(started.elapsed() >= Duration::from_millis(200));
    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}
