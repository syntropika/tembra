//! A platform data port survives typed-import refusals inside its guest, and `Complete` keeps
//! meaning that the callback physically ended.
//!
//! Breaking: the private stream request packets that a source bundle could forge on a data port
//! no longer exist. A prepared source Orb (`tests/source/stream-port-probe`) reaches its
//! dependency only through the typed WIT import of a reference the host issued and the SDK
//! selected in the same turn. On a data port the embedding answers those host requests. A stream
//! import or export never reaches a data port at all: opening one is refused (see
//! `source_streams::platform_data_ports_refuse_source_stream_*`).
#[path = "support/source.rs"]
mod source;
use serde_json::{Value, json};
use source::fixture;
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};
use tembra_executor_wasm::{Engine, InterpreterPort, Limits, PortEvent};

const MATH: &str = "tembra-test:port-refusal/math@1.0.0";
/// The reference the embedding issues for the factory's `Declaration.get(MATH)`.
const REFERENCE: u64 = 7;

/// Run one exchange to its completion, answering each host request with `answer`; returns the
/// result and every host packet the guest issued.
fn invoke(
    port: &InterpreterPort,
    method: &str,
    args: Value,
    asynchronous: bool,
    answer: &mut dyn FnMut(&Value) -> tembra::Result<Value>,
) -> (Value, Vec<Value>) {
    let (ticket, events) = port
        .begin(
            json!({"kind":"invoke", "environment":2, "service":"probe",
                "method":method, "args":args})
            .to_string(),
        )
        .unwrap();
    let mut packets = Vec::new();
    let result = loop {
        let event = if asynchronous {
            futures::executor::block_on(events.receive_async())
        } else {
            events.receive()
        }
        .expect("refusals happen inside the guest and never end the event receiver");
        match event {
            PortEvent::Host { packet, reply, .. } => {
                let packet: Value = serde_json::from_str(&packet).unwrap();
                reply.complete(answer(&packet).map(|value| value.to_string()));
                packets.push(packet);
            }
            PortEvent::Complete(result) => break result,
            PortEvent::Ingress(_) => panic!("the probe serves no incoming HTTP"),
        }
    };
    ticket
        .finish()
        .expect("Complete must mean the callback has physically ended");
    (serde_json::from_str(&result.unwrap()).unwrap(), packets)
}

/// The embedding's answers: the issued reference's status, and the dependency's `double`.
fn platform(refuse_status: bool) -> impl FnMut(&Value) -> tembra::Result<Value> {
    move |packet| match packet["operation"].as_str() {
        Some("reference.status") => {
            assert_eq!(packet["id"], json!(REFERENCE), "{packet}");
            if refuse_status {
                Err(tembra::Error::new(
                    tembra::core::Code::Unavailable,
                    "reference_released",
                    "the reference was released",
                ))
            } else {
                Ok(json!("ready"))
            }
        }
        Some("reference.call") => Ok(json!(packet["args"][0].as_u64().unwrap() * 2)),
        _ => panic!("unexpected host request {packet}"),
    }
}

/// Exactly one typed `reference.call` of the selected reference, naming its binding.
fn called(packets: &[Value], value: u64) -> bool {
    let calls: Vec<&Value> = packets
        .iter()
        .filter(|packet| packet["operation"] == "reference.call")
        .collect();
    calls.len() == 1
        && calls[0]["id"] == json!(REFERENCE)
        && calls[0]["binding"] == MATH
        && calls[0]["method"] == "double"
        && calls[0]["args"] == json!([value])
}

#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn typed_import_refusals_leave_data_ports_usable_after_physical_completion() {
    let artifact = tembra_contracts::Artifact::decode(&fixture("stream-port-probe")).unwrap();
    let bindings = artifact
        .contract
        .providers(
            &BTreeMap::from([(
                MATH.to_owned(),
                tembra_contracts::OrbProvider {
                    orb: "doubler".into(),
                    api_version: 1,
                },
            )]),
            &Default::default(),
        )
        .unwrap();
    for interpreted in [false, true] {
        let engine = if interpreted {
            Engine::interpreted(Limits::default())
        } else {
            Engine::new(Limits::default())
        }
        .unwrap();
        let port = engine
            .open_port(
                &artifact,
                &bindings,
                &Default::default(),
                &Default::default(),
            )
            .unwrap();
        let mut declared = Vec::new();
        for packet in [
            json!({"kind":"create", "environment":1, "service":null}),
            json!({"kind":"create", "environment":2, "service":"probe"}),
        ] {
            port.declare(packet, |packet, _| {
                let packet: Value = serde_json::from_str(packet).unwrap();
                declared.push(packet.clone());
                Ok(match packet["operation"].as_str() {
                    Some("reference.declare") => json!(REFERENCE),
                    _ => json!(1),
                })
            })
            .unwrap();
        }
        assert!(
            declared
                .contains(&json!({"operation":"reference.declare","service":MATH,"key":["main"]})),
            "interpreted={interpreted}: {declared:?}"
        );
        // Exercise both platform receive entrypoints, then reuse the same revision.
        for asynchronous in [false, true] {
            let mode = format!("interpreted={interpreted}, asynchronous={asynchronous}");
            // An argument outside the import's `u8` is refused before the host is called; the
            // selection is cleared, so the valid call of the same callback is the only call.
            let (result, packets) = invoke(
                &port,
                "forge",
                json!([0]),
                asynchronous,
                &mut platform(false),
            );
            let result = result.as_str().unwrap().to_owned();
            assert!(result.starts_with("RangeError"), "{mode}: {result}");
            assert!(result.ends_with("| 42"), "{mode}: {result}");
            assert!(called(&packets, 21), "{mode}: {packets:?}");
            // The platform refuses the selected reference: the guest catches the refusal, and
            // no typed call is issued.
            let (result, packets) = invoke(
                &port,
                "forge",
                json!([1]),
                asynchronous,
                &mut platform(true),
            );
            let result = result.as_str().unwrap().to_owned();
            assert!(result.contains("released"), "{mode}: {result}");
            assert!(!result.starts_with("accepted"), "{mode}: {result}");
            assert!(
                packets
                    .iter()
                    .all(|packet| packet["operation"] != "reference.call"),
                "{mode}: {packets:?}"
            );
            let (result, packets) =
                invoke(&port, "ping", json!([]), asynchronous, &mut platform(false));
            assert_eq!(result, json!(42), "{mode}");
            assert!(packets.is_empty(), "{mode}: {packets:?}");
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
