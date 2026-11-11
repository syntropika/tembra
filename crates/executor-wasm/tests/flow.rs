//! Run a real SDK-built source component against a native provider, never a mocked VM.
mod support;
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::rc::Rc;
use tembra::Host;
use tembra_executor_wasm::{Engine, Install, Limits};

/// Exercise WIT, TypeScript, persistent state, resource activation and native interop together.
/// An uncaught exception in a function without a WIT error lane traps and quarantines the
/// revision (declared breaking semantics); every other path runs before it.
#[test]
#[ignore = "requires TEMBRA_WASM_ORB from the reproducible build"]
fn real_source_component_flow() {
    let bytes =
        std::fs::read(std::env::var_os("TEMBRA_WASM_ORB").expect("TEMBRA_WASM_ORB")).unwrap();
    let engine = Engine::new(Limits::default()).unwrap();
    let install =
        || Install::default().requires("example:integrated/arithmetic@1.0.0", "native.math", 1);
    let mut corrupted = bytes.clone();
    corrupted.push(0xff);
    assert!(engine.load(&corrupted, install()).is_err());
    assert_eq!(engine.instances(), 0);
    let orb = engine.load(&bytes, install()).unwrap();
    let native = support::native_math("native.math");
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
            assert_eq!(
                calls
                    .call::<_, Value>(&context, "recover", json!([]))
                    .await?,
                42
            );
            assert_eq!(
                calls.call::<_, Value>(&context, "read", json!([])).await?,
                12
            );
            // The retired private host protocol is absent: probing it is an uncaught
            // exception, which quarantines the revision; later calls are refused.
            let forbidden = calls
                .call::<_, Value>(&context, "forbidden", json!([]))
                .await
                .unwrap_err();
            assert!(forbidden.message.contains("trap"), "{forbidden}");
            let after = calls
                .call::<_, Value>(&context, "fail", json!([]))
                .await
                .unwrap_err();
            assert_eq!(after.reason, "wasm_executor_quarantined", "{after}");
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
    let _ = reference.release();
    // Closing may report the quarantined revision; its Store is retired either way.
    let _ = pool.run_until(host.close());
    assert_eq!(engine.instances(), 0);
}
