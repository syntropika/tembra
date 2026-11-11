//! Unsupported private stream requests must not impersonate a port's physical completion.
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use tembra_executor_wasm::{Contract, Engine, InterpreterPort, Limits, PortEvent};

const WIT: &str = r#"
package tembra-test:port-refusal;
interface probe {
    forge: async func() -> string;
    ping: func() -> u32;
}
world orb { export probe; }
"#;

// Author code can reach __hostAsync even without importing WASI. Opcode 10 carries the
// well-formed JSON string "{}"; this port must refuse it and keep driving the callback.
const SOURCE: &str = r#"
globalThis.__tembraOrb = { services: { probe: { methods: {
    async forge() {
        try {
            await __hostAsync(Uint8Array.of(0, 10, 2, 0, 0, 0, 123, 125));
            return "accepted";
        } catch (error) {
            return error.message;
        }
    },
    ping() { return 42; },
} } } };
"#;

/// Call a probe method by its export slot, the function id every host sends.
fn invoke(port: &InterpreterPort, method: &str, asynchronous: bool) -> Value {
    let function = Contract::from_source("port-refusal.wit", WIT, Some("orb"))
        .unwrap()
        .bind("tembra-test:port-refusal/probe", method, true)
        .unwrap();
    let (ticket, events) = port
        .begin(
            json!({"kind":"invoke", "environment":2, "function":function, "args":[]}).to_string(),
        )
        .unwrap();
    let event = if asynchronous {
        futures::executor::block_on(events.receive_async())
    } else {
        events.receive()
    }
    .expect("unsupported requests must be refused inside the guest, not end the event receiver");
    let PortEvent::Complete(result) = event else {
        panic!("the probe must not issue embedding callbacks")
    };
    let result = serde_json::from_str(&result.unwrap()).unwrap();
    ticket
        .finish()
        .expect("Complete must mean the callback has physically ended");
    result
}

#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE"]
fn forged_stream_imports_leave_data_ports_usable_after_physical_completion() {
    let interpreter = std::fs::read(
        std::env::var_os("TEMBRA_WASM_ENGINE").expect("TEMBRA_WASM_ENGINE is required"),
    )
    .unwrap();
    let bytes = Contract::from_source("port-refusal.wit", WIT, Some("orb"))
        .unwrap()
        .source_artifact(SOURCE, "port-refusal", 1)
        .unwrap();
    let artifact = tembra_contracts::Artifact::decode(&bytes).unwrap();
    for interpreted in [false, true] {
        let engine = if interpreted {
            Engine::interpreted(Limits::default())
        } else {
            Engine::new(Limits::default())
        }
        .unwrap()
        .interpreter(&interpreter)
        .unwrap();
        let port = engine
            .open_port(
                &artifact,
                &Default::default(),
                &Default::default(),
                &Default::default(),
            )
            .unwrap();
        for packet in [
            json!({"kind":"create", "environment":1, "service":null}),
            json!({"kind":"create", "environment":2, "service":"probe"}),
        ] {
            port.declare(packet, |_, _| panic!("the probe declares no imports"))
                .unwrap();
        }
        // Exercise both platform receive entrypoints, then reuse the same revision.
        for asynchronous in [false, true] {
            let refusal = invoke(&port, "forge", asynchronous);
            assert!(
                refusal.as_str().unwrap().contains("native facade"),
                "interpreted={interpreted}, asynchronous={asynchronous}: {refusal}"
            );
            assert_eq!(invoke(&port, "ping", asynchronous), json!(42));
        }
        port.release(2);
        port.release(1);
        drop(port);
        let deadline = Instant::now() + Duration::from_secs(5);
        while engine.instances() != 0 {
            assert!(Instant::now() < deadline, "the port's Store did not retire");
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(engine.resources(), 0);
    }
}
