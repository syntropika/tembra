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

const RETHROW_WIT: &str = r#"
package tembra-test:port-rethrow;
interface probe {
    later: async func(mode: string) -> u32;
    now: func(mode: string) -> u32;
}
world orb { export probe; }
"#;

// Each method fails with the embedding's refusal of its host request: unchanged, or as a new
// error with the same text.
const RETHROW_SOURCE: &str = r#"
const fail = (mode, error) => { throw mode === "forged" ? new Error(error.message) : error; };
globalThis.__tembraOrb = { services: { probe: { methods: {
    async later(environment, context, mode) {
        try { await tembra.wait(1); } catch (error) { fail(mode, error); }
        return 0;
    },
    now(environment, context, mode) {
        try { __hostSync('{"operation":"orb"}'); } catch (error) { fail(mode, error); }
        return 0;
    },
} } } };
"#;

/// A source Orb served through a platform port that fails with the embedding's refusal of its host
/// request unchanged, synchronous or asynchronous, answers that refusal's code, as through the
/// native facade; one that throws its own error with the same text answers the executor's.
#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE"]
fn a_rethrown_refusal_keeps_its_code_through_a_platform_port() {
    let interpreter = std::fs::read(
        std::env::var_os("TEMBRA_WASM_ENGINE").expect("TEMBRA_WASM_ENGINE is required"),
    )
    .unwrap();
    let contract = Contract::from_source("port-rethrow.wit", RETHROW_WIT, Some("orb")).unwrap();
    let artifact = tembra_contracts::Artifact::decode(
        &contract
            .source_artifact(RETHROW_SOURCE, "port-rethrow", 1)
            .unwrap(),
    )
    .unwrap();
    let engine = Engine::new(Limits::default())
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
    for (method, mode, expected) in [
        (
            "later",
            "rethrown",
            (tembra::Code::Forbidden, "bridge_closed"),
        ),
        (
            "now",
            "rethrown",
            (tembra::Code::Forbidden, "bridge_closed"),
        ),
        (
            "later",
            "forged",
            (tembra::Code::Unavailable, "wasm_executor"),
        ),
        (
            "now",
            "forged",
            (tembra::Code::Unavailable, "wasm_executor"),
        ),
    ] {
        let function = contract
            .bind("tembra-test:port-rethrow/probe", method, true)
            .unwrap();
        let packet = json!({"kind":"invoke", "environment":2, "function":function, "args":[mode]});
        let (ticket, events) = port.begin(packet.to_string()).unwrap();
        let result = loop {
            match events.receive().unwrap() {
                PortEvent::Host { reply, .. } => {
                    reply.complete(Err(tembra::Error::new(
                        tembra::Code::Forbidden,
                        "bridge_closed",
                        "the bridge refuses",
                    )));
                }
                PortEvent::Complete(result) => break result,
                PortEvent::Ingress(_) => panic!("no incoming HTTP exchange"),
            }
        };
        ticket.finish().unwrap();
        let error = result.unwrap_err();
        assert_eq!(
            (error.code, error.reason.as_str()),
            expected,
            "{method} {mode}: {error}"
        );
    }
    port.release(2);
    port.release(1);
}
