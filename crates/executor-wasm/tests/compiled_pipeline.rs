//! A real compiled Rust Orb imports two prepared TypeScript Orbs, one with delayed results.
//!
//! The source providers are `tests/source/test-pipeline-{arithmetic,delayed}`. A synchronous WIT
//! function is implemented synchronously, so only the P3 async `delayed` provider waits; the
//! synchronous `arithmetic` provider answers the compiled guest's sync lowering at once.
#[path = "support/source.rs"]
mod source;
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::{path::PathBuf, rc::Rc, time::Duration};
use tembra::Host;
use tembra_executor_wasm::{Contract, Install};

const PIPELINE: &str = "example:pipeline/pipeline@1.0.0";

/// Resolve one world of the pipeline test contract.
fn contract(world: &str) -> Contract {
    Contract::from_dir(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/wit/pipeline"),
        Some(world),
    )
    .unwrap()
}

/// Source results reach the compiled guest through sync and delayed P3 async imports.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES and TEMBRA_WASM_CONSUMER"]
fn compiled_rust_orb_awaits_delayed_typescript_dependencies() {
    let component =
        std::fs::read(std::env::var_os("TEMBRA_WASM_CONSUMER").expect("TEMBRA_WASM_CONSUMER"))
            .unwrap();
    for engine in source::engines() {
        let arithmetic = engine
            .load(
                &source::fixture("test-pipeline-arithmetic"),
                Install::default(),
            )
            .unwrap();
        let delayed = engine
            .load(
                &source::fixture("test-pipeline-delayed"),
                Install::default(),
            )
            .unwrap();
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
                // Two synchronous `arithmetic.double` calls.
                assert_eq!(
                    called
                        .call::<_, Value>(&context, "quadruple", json!([6]))
                        .await?,
                    24
                );
                // Three sequential awaited `delayed.double` calls.
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
        // Three sequential async dependency calls each waited inside their source provider.
        assert!(started.elapsed() >= Duration::from_millis(120));
        reference.release().unwrap();
        pool.run_until(host.close()).unwrap();
        assert_eq!(engine.instances(), 0);
        assert!(host.errors().is_empty(), "{:?}", host.errors());
    }
}
