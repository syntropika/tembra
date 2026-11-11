//! Cross two actual Wasm Stores: TypeScript/dquickts and a compiled Rust WIT provider.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::rc::Rc;
use tembra::Host;
use tembra_executor_wasm::{Contract, Engine, Install, Limits};

/// Qualify real Canonical ABI calls and reject a component with a different declared signature.
#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE, TEMBRA_WASM_ORB and TEMBRA_WASM_PROVIDER"]
fn typescript_calls_a_compiled_rust_orb() {
    let interpreter =
        std::fs::read(std::env::var_os("TEMBRA_WASM_ENGINE").expect("TEMBRA_WASM_ENGINE")).unwrap();
    let component =
        std::fs::read(std::env::var_os("TEMBRA_WASM_PROVIDER").expect("TEMBRA_WASM_PROVIDER"))
            .unwrap();
    let bytes =
        std::fs::read(std::env::var_os("TEMBRA_WASM_ORB").expect("TEMBRA_WASM_ORB")).unwrap();
    let engine = Engine::new(Limits::default())
        .unwrap()
        .interpreter(&interpreter)
        .unwrap();
    let wrong = Contract::from_source("wrong.wit","package example:integrated@1.0.0; interface arithmetic { double: func(value:string)->s32; } world provider { export arithmetic; }",None).unwrap();
    assert!(
        wrong
            .component_artifact(&component, "math", 1, &Default::default())
            .is_err()
    );
    assert_eq!(engine.instances(), 0);
    let provider = Contract::from_dir(
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/wasm-orb"),
        Some("arithmetic-provider"),
    )
    .unwrap();
    let math = engine
        .load(
            &provider
                .component_artifact(&component, "math", 1, &Default::default())
                .unwrap(),
            Install::default(),
        )
        .unwrap();
    let install = || Install::default().requires("example:integrated/arithmetic@1.0.0", "math", 1);
    let calculator = engine.load(&bytes, install()).unwrap();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![math, calculator]).unwrap();
    let reference = Rc::new(
        host.get(
            "calculator::example:integrated/calculation@1.0.0",
            vec!["main".into()],
            "alice",
        )
        .unwrap(),
    );
    pool.run_until(reference.ready()).unwrap();
    assert_eq!(engine.instances(), 2);
    let called = reference.clone();
    pool.run_until(host.root("alice", move |context| {
        async move {
            assert_eq!(
                called
                    .call::<_, Value>(&context, "sum", json!([{"values":[3,4,5]}]))
                    .await?,
                24
            );
            assert_eq!(
                called.call::<_, Value>(&context, "read", json!([])).await?,
                24
            );
            assert_eq!(
                called
                    .call::<_, Value>(&context, "resource-value", json!([]))
                    .await?,
                42
            );
            Ok(())
        }
        .boxed_local()
    }))
    .unwrap();
    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
    assert!(host.errors().is_empty());
}
