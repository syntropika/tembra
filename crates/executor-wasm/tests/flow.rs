//! Run with an explicitly built interpreter and prepared source, never a mocked VM.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::{collections::BTreeMap, rc::Rc};
use tembra::{Host, Orb, Service};
use tembra_executor_wasm::{Binding, Engine, Grants, Limits, PreparedSource};

/// Exercise WIT, TypeScript, persistent state, resource activation and native interop together.
#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE and TEMBRA_WASM_ORB from the reproducible build"]
fn real_interpreter_flow() {
    let component =
        std::fs::read(std::env::var_os("TEMBRA_WASM_ENGINE").expect("TEMBRA_WASM_ENGINE")).unwrap();
    let prepared: PreparedSource = serde_json::from_slice(
        &std::fs::read(std::env::var_os("TEMBRA_WASM_ORB").expect("TEMBRA_WASM_ORB")).unwrap(),
    )
    .unwrap();
    let engine = Engine::new(&component, Limits::default()).unwrap();
    let bindings = BTreeMap::from([(
        "example:integrated/arithmetic@1.0.0".into(),
        Binding::Orb {
            name: "native.math".into(),
            api_version: 1,
            world: "arithmetic-provider".into(),
        },
    )]);
    let mut corrupted = prepared.clone();
    corrupted.source.push_str("\nthrow new Error('tampered');");
    assert!(
        engine
            .prepare_source("calculator", 1, &corrupted, &bindings, &Grants::default())
            .is_err()
    );
    assert_eq!(engine.instances(), 0);
    let orb = engine
        .prepare_source("calculator", 1, &prepared, &bindings, &Grants::default())
        .unwrap();
    let native = Orb::new("native.math", 1).service(
        Service::new("native.math::example:integrated/arithmetic@1.0.0", |_| {
            Ok(())
        })
        .method(
            "double",
            |_, _, (value,): (i32,)| async move { Ok(value * 2) },
        ),
    );
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![native, orb]).unwrap();
    assert_eq!(engine.instances(), 1);
    let reference = Rc::new(
        host.get(
            "calculator::example:integrated/calculation@1.0.0",
            vec!["main".into()],
            "alice",
        )
        .unwrap(),
    );
    pool.run_until(reference.ready()).unwrap();
    let calls = reference.clone();
    pool.run_until(host.root("alice", move |context| {
        async move {
            let value: Value = calls
                .call(&context, "sum", json!([{"values":[1,2,3]}]))
                .await?;
            assert_eq!(value, 12);
            let value: Value = calls.call(&context, "read", json!([])).await?;
            assert_eq!(value, 12);
            let value: Value = calls.call(&context, "resource-value", json!([])).await?;
            assert_eq!(value, 42);
            assert!(
                calls
                    .call::<_, Value>(&context, "fail", json!([]))
                    .await
                    .is_err()
            );
            assert_eq!(
                calls
                    .call::<_, Value>(&context, "recover", json!([]))
                    .await?,
                42
            );
            assert!(
                calls
                    .call::<_, Value>(&context, "forbidden", json!([]))
                    .await
                    .is_err()
            );
            assert_eq!(
                calls.call::<_, Value>(&context, "read", json!([])).await?,
                12
            );
            Ok(())
        }
        .boxed_local()
    }))
    .unwrap();
    assert_eq!(
        reference
            .state::<Value>()
            .unwrap()
            .value()
            .unwrap()
            .as_i64(),
        Some(12)
    );
    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
    assert!(host.errors().is_empty());
}
