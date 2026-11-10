//! A compiled component's dependency calls cross the portable mobile transport as ordinary
//! scoped host events. The test plays the platform scope that the SDK `Scope` normally serves.
use futures::executor::block_on;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tembra_mobile::{NativeArtifact, NativeInterpreter, NativePort};

#[path = "../../executor-wasm/tests/support/components.rs"]
mod components;
use components::*;

/// Answer one platform request: `(packet, asynchronous) -> reply JSON envelope`.
type Scope<'a> = &'a mut dyn FnMut(&Value, bool) -> String;

/// Drive one exchange to completion, answering every host request through `scope`.
fn exchange(port: &Arc<NativePort>, command: Value, declaration: bool, scope: Scope<'_>) -> Value {
    port.begin(command.to_string(), declaration).unwrap();
    loop {
        let event: Value = serde_json::from_str(&block_on(port.receive()).unwrap()).unwrap();
        if event["event"] == "complete" {
            port.finish().unwrap();
            return event["result"].clone();
        }
        let packet: Value = serde_json::from_str(event["packet"].as_str().unwrap()).unwrap();
        let reply = scope(&packet, event["asynchronous"] == true);
        port.reply(event["id"].as_u64().unwrap() as u32, reply)
            .unwrap();
    }
}

/// Open the compiled consumer with its math import bound to an Orb named `math`.
fn consumer(backend: &NativeInterpreter) -> Arc<NativePort> {
    let artifact = NativeArtifact::new(artifact("consumer", CONSUMER, "consumer")).unwrap();
    let plan: Value = serde_json::from_str(
        &artifact.request(
            json!({"kind":"install","name":"consumer","apiVersion":1,
            "requires":{MATH:{"orb":"math","apiVersion":1}},
            "grants":{},"filesystem":[],"network":[]})
            .to_string(),
        ),
    )
    .unwrap();
    backend
        .open(
            artifact,
            plan["ok"]["bindings"].to_string(),
            "{}".into(),
            None,
        )
        .unwrap()
}

/// The platform scope declares the default-key reference, then serves delayed dependency calls.
#[test]
fn portable_compiled_imports_use_scoped_host_events() {
    let backend = NativeInterpreter::new("{}".into(), vec![]).unwrap();
    let port = consumer(&backend);
    let description: Value = serde_json::from_str(&port.description().unwrap()).unwrap();
    assert_eq!(
        description["services"]["calc"]["methods"],
        json!(["quadruple"])
    );
    let mut declared = Vec::new();
    let root = exchange(
        &port,
        json!({"kind":"create","environment":1,"service":null}),
        true,
        &mut |packet, asynchronous| {
            assert!(!asynchronous);
            declared.push(packet.clone());
            json!({"ok": 7}).to_string()
        },
    );
    assert_eq!(root, json!({"ok":"null"}));
    assert_eq!(
        declared,
        vec![json!({"operation":"reference.declare","service":MATH,"key":["main"]})]
    );
    let service = exchange(
        &port,
        json!({"kind":"create","environment":2,"service":"calc"}),
        true,
        &mut |packet, _| {
            assert_eq!(packet, &json!({"operation":"orb"}));
            json!({"ok": 1}).to_string()
        },
    );
    assert_eq!(service, json!({"ok":"null"}));
    let mut calls = Vec::new();
    let result = exchange(
        &port,
        json!({"kind":"invoke","environment":2,"service":"calc","method":"quadruple","args":[5]}),
        false,
        &mut |packet, asynchronous| {
            assert!(asynchronous);
            calls.push(packet.clone());
            // A real platform provider answers later; the Store keeps waiting without a thread.
            std::thread::sleep(Duration::from_millis(20));
            json!({"ok": packet["args"][0].as_u64().unwrap() * 2}).to_string()
        },
    );
    assert_eq!(result, json!({"ok":"20"}));
    assert_eq!(
        calls,
        vec![
            json!({"operation":"reference.call","scope":1,"id":7,"method":"double","args":[5]}),
            json!({"operation":"reference.call","scope":1,"id":7,"method":"double","args":[10]}),
        ]
    );
    assert_eq!(backend.instances(), 1);
    port.release(2).unwrap();
    port.release(1).unwrap();
    port.shutdown().unwrap();
    assert_eq!(backend.instances(), 0);
}

/// A refused dependency call traps the synchronous import and quarantines only this Store.
#[test]
fn portable_compiled_import_refusal_quarantines_the_importer() {
    let backend = NativeInterpreter::new("{}".into(), vec![]).unwrap();
    let port = consumer(&backend);
    exchange(
        &port,
        json!({"kind":"create","environment":1,"service":null}),
        true,
        &mut |_, _| json!({"ok": 7}).to_string(),
    );
    let result = exchange(
        &port,
        json!({"kind":"invoke","environment":1,"service":"calc","method":"quadruple","args":[5]}),
        false,
        &mut |_, _| json!({"error":"provider refused"}).to_string(),
    );
    let message = result["error"]["message"].as_str().unwrap();
    assert!(message.contains("component trap"), "{message}");
    assert!(message.contains("provider refused"), "{message}");
    assert_eq!(backend.instances(), 0);
    let error = port
        .begin(
            json!({"kind":"invoke","environment":1,"service":"calc","method":"quadruple","args":[1]})
                .to_string(),
            false,
        )
        .unwrap_err();
    assert!(error.to_string().contains("component trap"), "{error}");
    port.shutdown().unwrap();
}
