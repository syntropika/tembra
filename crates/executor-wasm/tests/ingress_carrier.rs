//! The typed platform carrier serves a compiled handler through `InterpreterPort`, as Node and
//! mobile do: control packets stay JSON, while heads, bodies and trailers are raw bytes pulled
//! one at a time in each direction.
use std::{sync::mpsc, thread, time::Duration};
use tembra_executor_wasm::{
    Contract, Engine, Grants, InterpreterPort, Limits, PortEvent, PortReply, PortTicket,
    ingress::{
        IngressEvent, IngressField, IngressPort, IngressRequestChunk, IngressRequestHead,
        IngressResponseFrame,
    },
};

const ORB: &str = "web";

fn fixture() -> Option<Vec<u8>> {
    let Some(path) = std::env::var_os("TEMBRA_WASM_HTTP_HANDLER") else {
        eprintln!("skipped: TEMBRA_WASM_HTTP_HANDLER names no compiled HTTP handler");
        return None;
    };
    Some(std::fs::read(path).unwrap())
}

/// Open the handler revision through the platform port and declare its environments the way
/// a platform kernel does: the Orb root, then the handler service.
fn open(engine: &Engine, component: &[u8]) -> InterpreterPort {
    let bytes = Contract::from_dir(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/wit/handler"),
        Some("handler"),
    )
    .unwrap()
    .component_artifact(component, ORB, 1, &Default::default())
    .unwrap();
    let artifact = tembra_contracts::Artifact::decode(&bytes).unwrap();
    let bindings = artifact
        .contract
        .providers(&Default::default(), &Grants::default())
        .unwrap();
    let port = engine
        .open_port(
            &artifact,
            &bindings,
            &Grants::default(),
            &Default::default(),
        )
        .unwrap();
    for packet in [
        serde_json::json!({"kind": "create", "environment": 1, "service": null}),
        serde_json::json!({"kind": "create", "environment": 2, "service": "handler"}),
    ] {
        port.declare(packet, |_, _| Ok(serde_json::Value::Null))
            .unwrap();
    }
    port
}

fn field(name: &str, value: &[u8]) -> IngressField {
    IngressField {
        name: name.into(),
        value: value.to_vec(),
    }
}

fn head(path: &str) -> IngressRequestHead {
    IngressRequestHead {
        method: "POST".into(),
        scheme: Some("http".into()),
        authority: Some("ingress.test".into()),
        path_with_query: Some(path.into()),
        fields: vec![field("x-dup", b"1"), field("x-dup", b"\xff2")],
    }
}

const PACKET: &str = r#"{"kind":"ingress","environment":2,"service":"handler","method":"handle"}"#;

enum Ev {
    Host(String, PortReply),
    Complete(tembra::Result<String>),
    Ingress(IngressEvent),
}

/// One platform exchange: its owner ticket, its carrier port and its forwarded events.
struct Exchange {
    ticket: Option<PortTicket>,
    port: IngressPort,
    events: mpsc::Receiver<Ev>,
    /// Request-body pulls announced and not yet answered.
    pulls: usize,
}

impl Exchange {
    fn begin(port: &InterpreterPort, path: &str) -> Self {
        let (ticket, events, carrier) = port.begin_ingress(PACKET.into(), head(path)).unwrap();
        let (send, receive) = mpsc::channel();
        thread::spawn(move || {
            loop {
                let event = match events.receive().unwrap() {
                    PortEvent::Host { reply, packet, .. } => Ev::Host(packet, reply),
                    PortEvent::Complete(result) => {
                        let _ = send.send(Ev::Complete(result));
                        return;
                    }
                    PortEvent::Ingress(event) => Ev::Ingress(event),
                };
                if send.send(event).is_err() {
                    return;
                }
            }
        });
        Self {
            ticket: Some(ticket),
            port: carrier,
            events: receive,
            pulls: 0,
        }
    }

    /// The next ingress event or completion; host requests are refused (none are expected).
    fn next(&mut self) -> Ev {
        loop {
            match self.events.recv_timeout(Duration::from_secs(10)).unwrap() {
                Ev::Host(packet, reply) => answer(&packet, reply),
                Ev::Ingress(IngressEvent::RequestPull) => self.pulls += 1,
                other => return other,
            }
        }
    }

    fn frame(&mut self) -> IngressResponseFrame {
        self.port.pull_response().unwrap();
        match self.next() {
            Ev::Ingress(IngressEvent::Response(frame)) => frame,
            _ => panic!("expected a response frame"),
        }
    }

    /// Wait for the guest's request-body pull, then answer it.
    fn supply(&mut self, chunk: IngressRequestChunk) -> bool {
        while self.pulls == 0 {
            match self.events.recv_timeout(Duration::from_secs(10)).unwrap() {
                Ev::Ingress(IngressEvent::RequestPull) => self.pulls += 1,
                Ev::Host(packet, reply) => answer(&packet, reply),
                _ => panic!("expected a request-body pull"),
            }
        }
        self.pulls -= 1;
        self.port.supply_request(chunk).unwrap()
    }

    fn complete(mut self) -> tembra::Result<String> {
        let result = loop {
            if let Ev::Complete(result) = self.next() {
                break result;
            }
        };
        self.ticket.take().unwrap().finish().unwrap();
        result
    }
}

/// Answer a control request as a platform kernel would: a source guest's quiescence check
/// waits one host round trip (`wait`); nothing else is expected.
fn answer(packet: &str, reply: PortReply) {
    let packet: serde_json::Value = serde_json::from_str(packet).unwrap();
    match packet["operation"].as_str() {
        Some("wait") => {
            reply.complete(Ok("null".into()));
        }
        operation => panic!("unexpected host request {operation:?}"),
    }
}

fn data(text: &str) -> IngressResponseFrame {
    IngressResponseFrame::Data(format!("data: {text}\n\n").into_bytes())
}

fn engines() -> [Engine; 2] {
    [
        Engine::new(Limits::default()).unwrap(),
        Engine::interpreted(Limits::default()).unwrap(),
    ]
}

/// The status route's `x-last-exit`, served through the carrier: the revision is still usable.
fn last_exit(port: &InterpreterPort) -> String {
    let mut exchange = Exchange::begin(port, "/status");
    let Ev::Ingress(IngressEvent::Head(head)) = exchange.next() else {
        panic!("expected a head")
    };
    assert_eq!(exchange.frame(), IngressResponseFrame::End);
    exchange.complete().unwrap();
    let exit = head
        .fields
        .iter()
        .find(|field| field.name == "x-last-exit")
        .unwrap();
    String::from_utf8(exit.value.clone()).unwrap()
}

/// A handler `error-code` through the carrier: one typed `Refused` before the completion, never
/// a head. With `delay`, the platform reads its events late; the order must not change.
fn refusal(port: &InterpreterPort, path: &str, delay: Duration) -> Vec<u8> {
    let mut exchange = Exchange::begin(port, path);
    thread::sleep(delay);
    let Ev::Ingress(IngressEvent::Refused(code)) = exchange.next() else {
        panic!("{path}: the refusal precedes the completion and has no head")
    };
    let Ev::Complete(result) = exchange.next() else {
        panic!("{path}: one refusal, then the completion")
    };
    // The completion keeps reporting the refusal untyped; the typed value is the event.
    assert!(result.is_err(), "{path}");
    exchange.ticket.take().unwrap().finish().unwrap();
    code
}

/// Standard-codec oracles for the fixtures' typed refusals (variant index, then payload).
fn expected_refusals() -> Vec<(&'static str, Vec<u8>)> {
    fn text(out: &mut Vec<u8>, value: &str) {
        out.extend_from_slice(&(value.len() as u32).to_le_bytes());
        out.extend_from_slice(value.as_bytes());
    }
    let mut escaped = 38u32.to_le_bytes().to_vec();
    escaped.push(1);
    text(&mut escaped, "quote \" slash \\ line\nend é🌍");
    let mut record = 22u32.to_le_bytes().to_vec();
    record.extend_from_slice(&[1, 1]);
    text(&mut record, "x-big");
    record.push(1);
    record.extend_from_slice(&u32::MAX.to_le_bytes());
    let mut u64_max = 28u32.to_le_bytes().to_vec();
    u64_max.push(1);
    u64_max.extend_from_slice(&u64::MAX.to_le_bytes());
    let mut dns = 1u32.to_le_bytes().to_vec();
    dns.push(1);
    text(&mut dns, "SERVFAIL");
    dns.push(1);
    dns.extend_from_slice(&u16::MAX.to_le_bytes());
    vec![
        ("/refuse/escaped", escaped),
        ("/refuse/record", record),
        ("/refuse/u64", u64_max),
        ("/refuse/dns", dns),
    ]
}

#[test]
#[ignore = "requires TEMBRA_WASM_HTTP_HANDLER"]
fn compiled_refusals_arrive_typed_before_completion() {
    let Some(component) = fixture() else {
        return;
    };
    for engine in engines() {
        let port = open(&engine, &component);
        for (index, (path, expected)) in expected_refusals().into_iter().enumerate() {
            let delay = Duration::from_millis(if index == 0 { 300 } else { 0 });
            assert_eq!(refusal(&port, path, delay), expected, "{path}");
        }
        let mut refused = 38u32.to_le_bytes().to_vec();
        refused.push(1);
        refused.extend_from_slice(&7u32.to_le_bytes());
        refused.extend_from_slice(b"refused");
        assert_eq!(refusal(&port, "/error", Duration::ZERO), refused);
        // A refusal is the request's answer only: the Store keeps serving.
        last_exit(&port);
        drop(port);
        assert_eq!(engine.instances(), 0);
    }
}

#[test]
#[ignore = "requires TEMBRA_WASM_HTTP_HANDLER"]
fn the_platform_carrier_streams_bytes_both_ways() {
    let Some(component) = fixture() else {
        return;
    };
    for engine in engines() {
        let port = open(&engine, &component);
        let mut exchange = Exchange::begin(&port, "/sse");
        let Ev::Ingress(IngressEvent::Head(head)) = exchange.next() else {
            panic!("expected the head first")
        };
        assert_eq!(head.status, 200);
        assert!(
            head.fields
                .contains(&field("content-type", b"text/event-stream"))
        );
        // The first event arrives while the request body is still open.
        assert_eq!(exchange.frame(), data("open"));
        assert!(exchange.supply(IngressRequestChunk::Data(b"alpha".to_vec())));
        assert_eq!(exchange.frame(), data("alpha"));
        // Protocol misuse is refused: a second pull before its frame, a second answer to one pull.
        exchange.port.pull_response().unwrap();
        assert!(exchange.port.pull_response().is_err());
        assert!(exchange.supply(IngressRequestChunk::Data(b"beta".to_vec())));
        let Ev::Ingress(IngressEvent::Response(frame)) = exchange.next() else {
            panic!("expected the pending frame")
        };
        assert_eq!(frame, data("beta"));
        assert!(exchange.supply(IngressRequestChunk::End {
            trailers: vec![field("x-request", b"done")],
        }));
        assert_eq!(exchange.frame(), data("end"));
        assert_eq!(
            exchange.frame(),
            IngressResponseFrame::Trailers(vec![field("x-events", b"4")])
        );
        assert_eq!(exchange.frame(), IngressResponseFrame::End);
        // Nothing may be pulled after the end, and a late request answer owns nothing.
        assert!(exchange.port.pull_response().is_err());
        assert!(
            !exchange
                .port
                .supply_request(IngressRequestChunk::Data(b"late".to_vec()))
                .unwrap_or(false)
        );
        assert_eq!(exchange.complete().unwrap(), "null");
        assert!(last_exit(&port).starts_with("complete events=4 transmit=Ok"));
        drop(port);
        assert_eq!(engine.instances(), 0);
    }
}

#[test]
#[ignore = "requires TEMBRA_WASM_HTTP_HANDLER"]
fn the_carrier_refuses_invalid_bytes_before_admission() {
    let Some(component) = fixture() else {
        return;
    };
    let engine = Engine::new(Limits::default()).unwrap();
    let port = open(&engine, &component);
    for invalid in [
        IngressRequestHead {
            fields: vec![field("x-bad", b"line\nbreak")],
            ..head("/sse")
        },
        IngressRequestHead {
            fields: vec![field("bad name", b"v")],
            ..head("/sse")
        },
        IngressRequestHead {
            method: "BAD METHOD".into(),
            ..head("/sse")
        },
    ] {
        assert!(port.begin_ingress(PACKET.into(), invalid).is_err());
    }
    // An ordinary dispatch cannot reach the handler, and a non-ingress packet cannot use the
    // carrier.
    assert!(
        port.begin_ingress(r#"{"kind":"invoke"}"#.into(), head("/sse"))
            .is_err()
    );
    let handle = Contract::from_dir(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/wit/handler"),
        Some("handler"),
    )
    .unwrap()
    .bind("wasi:http/handler@0.3.0", "handle", true)
    .unwrap();
    let (ticket, events) = port
        .begin(
            serde_json::json!({"kind":"invoke","environment":2,"function":handle,"args":[null]})
                .to_string(),
        )
        .unwrap();
    let PortEvent::Complete(result) = events.receive().unwrap() else {
        panic!("expected completion")
    };
    assert!(result.is_err());
    ticket.finish().unwrap();
    // Invalid trailer bytes, or a platform body error, end the guest's request body with an
    // error rather than a protocol fault; the response still completes.
    for end in [
        IngressRequestChunk::End {
            trailers: vec![field("x-t", b"a\rb")],
        },
        IngressRequestChunk::Error("boom".into()),
    ] {
        let mut exchange = Exchange::begin(&port, "/sse");
        assert!(matches!(
            exchange.next(),
            Ev::Ingress(IngressEvent::Head(_))
        ));
        assert_eq!(exchange.frame(), data("open"));
        assert!(exchange.supply(end));
        assert_eq!(exchange.frame(), data("end"));
        assert!(matches!(
            exchange.frame(),
            IngressResponseFrame::Trailers(_)
        ));
        assert_eq!(exchange.frame(), IngressResponseFrame::End);
        exchange.complete().unwrap();
    }
    drop(port);
    assert_eq!(engine.instances(), 0);
}

#[test]
#[ignore = "requires TEMBRA_WASM_HTTP_HANDLER"]
fn cancellation_and_disconnect_end_the_carrier_and_late_answers_own_nothing() {
    let Some(component) = fixture() else {
        return;
    };
    for engine in engines() {
        let port = open(&engine, &component);
        // Cancellation while the guest waits for request data.
        let mut exchange = Exchange::begin(&port, "/sse");
        assert!(matches!(
            exchange.next(),
            Ev::Ingress(IngressEvent::Head(_))
        ));
        assert_eq!(exchange.frame(), data("open"));
        exchange.ticket.as_ref().unwrap().cancel();
        // A host-driven end: the pending pull's error frame and the completion may come in
        // either order; completion always ends the body.
        exchange.port.pull_response().unwrap();
        let completion = loop {
            match exchange.next() {
                Ev::Ingress(IngressEvent::Response(frame)) => {
                    assert!(matches!(frame, IngressResponseFrame::Error(_)), "{frame:?}");
                }
                Ev::Complete(result) => break result,
                _ => panic!("unexpected event"),
            }
        };
        assert!(
            !exchange
                .port
                .supply_request(IngressRequestChunk::Data(b"late".to_vec()))
                .unwrap_or(false)
        );
        completion.unwrap();
        exchange.ticket.take().unwrap().finish().unwrap();
        assert!(last_exit(&port).starts_with("request-ended"));

        // A client disconnect (the platform drops its carrier port).
        let mut exchange = Exchange::begin(&port, "/sse");
        assert!(matches!(
            exchange.next(),
            Ev::Ingress(IngressEvent::Head(_))
        ));
        assert_eq!(exchange.frame(), data("open"));
        exchange.port.disconnect();
        exchange.complete().unwrap();
        assert_eq!(
            last_exit(&port),
            "request-ended-then-write-dropped events=1"
        );

        // A handler error has no head: its typed refusal, then the completion after settlement.
        assert!(!refusal(&port, "/error", Duration::ZERO).is_empty());
        drop(port);
        assert_eq!(engine.instances(), 0);
    }
}

/// A client that goes away before the response head (the platform disconnects while the guest
/// waits for request data) ends the call normally: no head, no quarantine, and the revision
/// keeps serving with its Store intact.
#[test]
#[ignore = "requires TEMBRA_WASM_HTTP_HANDLER"]
fn a_disconnect_before_the_head_ends_the_call_without_quarantine() {
    let Some(component) = fixture() else {
        return;
    };
    for engine in engines() {
        let port = open(&engine, &component);
        let mut exchange = Exchange::begin(&port, "/slow");
        while exchange.pulls == 0 {
            match exchange
                .events
                .recv_timeout(Duration::from_secs(10))
                .unwrap()
            {
                Ev::Ingress(IngressEvent::RequestPull) => exchange.pulls += 1,
                Ev::Host(packet, reply) => answer(&packet, reply),
                _ => panic!("expected the guest's request-body pull before any head"),
            }
        }
        exchange.port.disconnect();
        assert!(
            !exchange
                .port
                .supply_request(IngressRequestChunk::Data(b"late".to_vec()))
                .unwrap()
        );
        let completion = loop {
            match exchange.next() {
                Ev::Complete(result) => break result,
                Ev::Ingress(IngressEvent::Head(_)) => panic!("no head after a disconnect"),
                _ => {}
            }
        };
        if let Err(error) = &completion {
            assert_ne!(error.reason, "wasm_executor_quarantined", "{error}");
        }
        exchange.ticket.take().unwrap().finish().unwrap();
        assert_eq!(engine.instances(), 1);
        assert!(last_exit(&port).starts_with("slow-request-ended"));
        drop(port);
        assert_eq!(engine.instances(), 0);
    }
}

/// A source handler streams through the same typed carrier: the request is issued into the
/// revision's standard table, the guest publishes its response once, and the callback lasts
/// until the delivery ended.
#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE and TEMBRA_WASM_SOURCE_HANDLER"]
fn source_handlers_stream_through_the_platform_carrier() {
    let (Some(interpreter), Some(artifact)) = (
        std::env::var_os("TEMBRA_WASM_ENGINE").map(|path| std::fs::read(path).unwrap()),
        std::env::var_os("TEMBRA_WASM_SOURCE_HANDLER").map(|path| std::fs::read(path).unwrap()),
    ) else {
        eprintln!("skipped: TEMBRA_WASM_ENGINE or TEMBRA_WASM_SOURCE_HANDLER is not set");
        return;
    };
    for engine in engines() {
        let engine = engine.interpreter(&interpreter).unwrap();
        let artifact = tembra_contracts::Artifact::decode(&artifact).unwrap();
        let bindings = artifact
            .contract
            .providers(&Default::default(), &Grants::default())
            .unwrap();
        let port = engine
            .open_port(
                &artifact,
                &bindings,
                &Grants::default(),
                &Default::default(),
            )
            .unwrap();
        for packet in [
            serde_json::json!({"kind": "create", "environment": 1, "service": null}),
            serde_json::json!({"kind": "create", "environment": 2, "service": "handler"}),
        ] {
            port.declare(packet, |packet, _| {
                let packet: serde_json::Value = serde_json::from_str(packet).unwrap();
                match packet["operation"].as_str() {
                    Some("orb") => Ok(serde_json::Value::Null),
                    operation => panic!("unexpected declaration request {operation:?}"),
                }
            })
            .unwrap();
        }
        let mut exchange = Exchange::begin(&port, "/sse");
        let Ev::Ingress(IngressEvent::Head(head)) = exchange.next() else {
            panic!("expected the head")
        };
        assert_eq!(head.status, 200);
        assert_eq!(exchange.frame(), data("open"));
        assert!(exchange.supply(IngressRequestChunk::Data(vec![0x80, 0, 0xff])));
        assert_eq!(
            exchange.frame(),
            IngressResponseFrame::Data(vec![0x80, 0, 0xff])
        );
        assert!(exchange.supply(IngressRequestChunk::End { trailers: vec![] }));
        assert_eq!(exchange.frame(), data("end"));
        assert_eq!(
            exchange.frame(),
            IngressResponseFrame::Trailers(vec![field("x-chunks", b"1")])
        );
        assert_eq!(exchange.frame(), IngressResponseFrame::End);
        assert_eq!(exchange.complete().unwrap(), "null");

        // A handler error has no head; its typed refusal is exactly what the compiled handler
        // sends for the same WIT values; a disconnect ends the next stream; the port keeps
        // serving.
        assert_eq!(
            refusal(&port, "/error", Duration::ZERO),
            15u32.to_le_bytes(),
            "http-request-denied"
        );
        for (path, expected) in expected_refusals() {
            assert_eq!(refusal(&port, path, Duration::ZERO), expected, "{path}");
        }
        let mut exchange = Exchange::begin(&port, "/sse");
        assert!(matches!(
            exchange.next(),
            Ev::Ingress(IngressEvent::Head(_))
        ));
        assert_eq!(exchange.frame(), data("open"));
        exchange.port.disconnect();
        exchange.complete().unwrap();
        drop(port);
        assert_eq!(engine.instances(), 0);
    }
}

/// A source bundle whose own code rebinds the writable ingress global, so its callback returns
/// without ever publishing a response. No interpreter change is involved: this is user code.
const SILENT_HANDLER: &str = r#"
globalThis.__tembraIngress = Object.freeze({ async serve() {} });
globalThis.__tembraOrb = {
  name: "web",
  apiVersion: 1,
  services: { handler: { methods: { async handle() {} } } },
};
"#;

/// A callback that ends without publishing a head completes with a clear error through the
/// carrier, never `ok` without a response, and the revision keeps serving.
#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE"]
fn a_source_callback_without_a_response_completes_with_an_error() {
    let Some(interpreter) =
        std::env::var_os("TEMBRA_WASM_ENGINE").map(|path| std::fs::read(path).unwrap())
    else {
        eprintln!("skipped: TEMBRA_WASM_ENGINE is not set");
        return;
    };
    let contract = Contract::from_dir(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/wit/handler"),
        Some("handler"),
    )
    .unwrap();
    let bytes =
        tembra_contracts::Artifact::package_source(ORB, 1, &contract, SILENT_HANDLER).unwrap();
    for engine in engines() {
        let engine = engine.interpreter(&interpreter).unwrap();
        let artifact = tembra_contracts::Artifact::decode(&bytes).unwrap();
        let bindings = artifact
            .contract
            .providers(&Default::default(), &Grants::default())
            .unwrap();
        let port = engine
            .open_port(
                &artifact,
                &bindings,
                &Grants::default(),
                &Default::default(),
            )
            .unwrap();
        for packet in [
            serde_json::json!({"kind": "create", "environment": 1, "service": null}),
            serde_json::json!({"kind": "create", "environment": 2, "service": "handler"}),
        ] {
            port.declare(packet, |_, _| Ok(serde_json::Value::Null))
                .unwrap();
        }
        for _ in 0..2 {
            let mut exchange = Exchange::begin(&port, "/");
            let Ev::Complete(result) = exchange.next() else {
                panic!("a callback without a response publishes no head")
            };
            let error = result.unwrap_err();
            assert!(error.message.contains("returned no response"), "{error}");
            assert_ne!(error.reason, "wasm_executor_quarantined", "{error}");
            exchange.ticket.take().unwrap().finish().unwrap();
        }
        drop(port);
        assert_eq!(engine.instances(), 0);
    }
}
