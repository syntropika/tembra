//! Qualify standard WASI capabilities through the same portable mobile component transport.
use futures::executor::block_on;
use serde_json::{Value, json};
use std::sync::Arc;
use tembra_contracts::Contract;
use tembra_mobile::{NativeArtifact, NativeInterpreter, NativePort};

/// Run one exchange; the probe issues no scoped host requests.
fn exchange(port: &Arc<NativePort>, command: Value, declaration: bool) -> Value {
    port.begin(command.to_string(), declaration).unwrap();
    let event: Value = serde_json::from_str(&block_on(port.receive()).unwrap()).unwrap();
    assert_eq!(event["event"], "complete", "{event}");
    port.finish().unwrap();
    event["result"].clone()
}

/// Call one probe method, addressed by its JavaScript spelling, and decode its WIT result.
fn call(port: &Arc<NativePort>, method: &str, path: &str) -> Value {
    let result = exchange(
        port,
        json!({"kind":"invoke","environment":1,"service":"probe","method":method,"args":[path]}),
        false,
    );
    let text = result["ok"]
        .as_str()
        .unwrap_or_else(|| panic!("{method}: {result}"));
    json!({"ok": serde_json::from_str::<Value>(text).unwrap()})
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
    let artifact = NativeArtifact::new(
        contract
            .component_artifact(&raw, "probe", 1, &Default::default())
            .unwrap(),
    )
    .unwrap();
    let backend = NativeInterpreter::new(
        json!({"filesystem":{"data":directory.path()}}).to_string(),
        vec![],
    )
    .unwrap();
    assert!(
        backend
            .open(artifact.clone(), "{}".into(), "{}".into())
            .is_err()
    );
    for writable in [false, true] {
        let grants =
            json!({"filesystem":[{"provider":"data","mount":"/data","writable":writable}]});
        let plan: Value = serde_json::from_str(&artifact.request(json!({"kind":"install","name":"probe","apiVersion":1,"requires":{},"grants":grants,"filesystem":["data"],"network":[]}).to_string())).unwrap();
        let port = backend
            .open(
                artifact.clone(),
                plan["ok"]["bindings"].to_string(),
                grants.to_string(),
            )
            .unwrap();
        exchange(
            &port,
            json!({"kind":"create","environment":1,"service":"probe"}),
            true,
        );
        assert_eq!(call(&port, "open", "/data/input")["ok"]["tag"], "ok");
        assert_eq!(call(&port, "open", "/data/../outside")["ok"]["tag"], "err");
        assert_eq!(
            call(&port, "createDirectory", "/data/child")["ok"]["tag"],
            if writable { "ok" } else { "err" }
        );
        if writable {
            assert!(directory.path().join("child").is_dir());
            assert_eq!(
                call(&port, "removeDirectory", "/data/child")["ok"]["tag"],
                "ok"
            );
        }
        port.release(1).unwrap();
        port.shutdown().unwrap();
        assert_eq!(backend.instances(), 0);
    }
}
