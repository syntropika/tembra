//! Qualify standard WASI capabilities through the same portable mobile component transport.
use futures::executor::block_on;
use serde_json::{Value, json};
use tembra_contracts::Contract;
use tembra_mobile::{NativeArtifact, NativeComponent, NativeInterpreter};

fn call(port: &NativeComponent, method: &str, path: &str) -> Value {
    serde_json::from_str(
        &block_on(port.call(
            "example:probe/probe@1.0.0".into(),
            method.into(),
            json!([path]).to_string(),
        ))
        .unwrap(),
    )
    .unwrap()
}

#[test]
#[ignore = "requires TEMBRA_WASM_PROBE"]
fn portable_compiled_filesystem_enforces_standard_preopen_permissions() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("input"), "mobile").unwrap();
    let raw = std::fs::read(std::env::var_os("TEMBRA_WASM_PROBE").unwrap()).unwrap();
    let contract = Contract::from_source(
        "probe.wit",
        include_str!("../../executor-wasm/tests/wit/probe/probe.wit"),
        Some("probe-provider"),
    )
    .unwrap();
    let artifact =
        NativeArtifact::new(contract.artifact(None, Some(&raw), "probe", 1).unwrap()).unwrap();
    let backend = NativeInterpreter::new(
        json!({"filesystem":{"data":directory.path()}}).to_string(),
        vec![],
    )
    .unwrap();
    assert!(
        backend
            .open_component(artifact.clone(), "{}".into(), "{}".into())
            .is_err()
    );
    for writable in [false, true] {
        let grants =
            json!({"filesystem":[{"provider":"data","mount":"/data","writable":writable}]});
        let plan: Value = serde_json::from_str(&artifact.request(json!({"kind":"install","name":"probe","apiVersion":1,"requires":{},"grants":grants,"filesystem":["data"],"network":[]}).to_string())).unwrap();
        let port = backend
            .open_component(
                artifact.clone(),
                plan["ok"]["bindings"].to_string(),
                grants.to_string(),
            )
            .unwrap();
        assert_eq!(call(&port, "open", "/data/input")["ok"]["tag"], "ok");
        assert_eq!(call(&port, "open", "/data/../outside")["ok"]["tag"], "err");
        assert_eq!(
            call(&port, "create-directory", "/data/child")["ok"]["tag"],
            if writable { "ok" } else { "err" }
        );
        if writable {
            assert!(directory.path().join("child").is_dir());
            assert_eq!(
                call(&port, "remove-directory", "/data/child")["ok"]["tag"],
                "ok"
            );
        }
        port.shutdown();
        assert_eq!(backend.instances(), 0);
    }
}
