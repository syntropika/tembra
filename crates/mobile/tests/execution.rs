//! Exercise the actual mobile transport and portable Wasmtime engine, without mocked guest execution.
use futures::executor::block_on;
use serde_json::{Value, json};
use std::sync::Arc;
use tembra_contracts::Contract;
use tembra_mobile::{NativeArtifact, NativeInterpreter, NativePort};

/// Drive one exchange like the SDK mobile driver: host requests are answered as they arrive and
/// the ticket finishes after the guest completed.
fn exchange(port: &Arc<NativePort>, command: Value, declaration: bool) -> Value {
    port.begin(command.to_string(), declaration).unwrap();
    loop {
        let event: Value = serde_json::from_str(&block_on(port.receive()).unwrap()).unwrap();
        if event["event"] == "complete" {
            port.finish().unwrap();
            assert!(
                event["result"].get("error").is_none(),
                "command {command}: {event}"
            );
            return serde_json::from_str(event["result"]["ok"].as_str().unwrap()).unwrap();
        }
        let packet: Value = serde_json::from_str(event["packet"].as_str().unwrap()).unwrap();
        // Standard WASI runs inside the Store; these Orbs issue no asynchronous host request.
        assert_ne!(
            event["asynchronous"], true,
            "unexpected host operation {packet}"
        );
        let value = match packet["operation"].as_str().unwrap() {
            // A service environment's Orb root; these tests create the root as environment 1.
            "orb" if command["service"].is_null() => Value::Null,
            "orb" => json!(1),
            "state.declare" => json!(1),
            // The SDK checks its admitted callback before standard HTTP sends.
            "context.check" => Value::Null,
            "address" => json!({"key":[],"principal":"test"}),
            operation => panic!("unexpected operation {operation}"),
        };
        assert!(
            port.reply(
                event["id"].as_u64().unwrap() as u32,
                json!({"ok":value}).to_string()
            )
            .unwrap()
        );
    }
}

/// Prepared source component fixtures (`TEMBRA_WASM_FIXTURES`), built by the SDK from
/// `packages/ts/sdk/test/fixtures/wasm-mobile` with the packaged runtime.
fn fixture(name: &str) -> Arc<NativeArtifact> {
    let directory = std::env::var_os("TEMBRA_WASM_FIXTURES")
        .expect("TEMBRA_WASM_FIXTURES names the prepared source fixture directory");
    let path = std::path::Path::new(&directory).join(format!("{name}.wasm"));
    let bytes = std::fs::read(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    NativeArtifact::new(bytes).unwrap()
}

#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn portable_source_transport_preserves_factories_and_isolates_heaps() {
    let artifact = fixture("mobile-counter");
    let backend = NativeInterpreter::new("{}".into()).unwrap();
    let first = backend
        .open(artifact.clone(), "{}".into(), "{}".into(), None)
        .unwrap();
    let second = backend
        .open(artifact.clone(), "{}".into(), "{}".into(), None)
        .unwrap();
    for port in [&first, &second] {
        exchange(
            port,
            json!({"kind":"create","environment":1,"service":null}),
            true,
        );
        exchange(
            port,
            json!({"kind":"create","environment":2,"service":"counter"}),
            true,
        );
    }
    let call =
        || json!({"kind":"invoke","environment":2,"service":"counter","method":"add","args":[]});
    assert_eq!(exchange(&first, call(), false), 1);
    assert_eq!(exchange(&first, call(), false), 2);
    assert_eq!(exchange(&second, call(), false), 1);
    assert_eq!(backend.instances(), 2);
    let logs: Value = serde_json::from_str(&first.take_logs()).unwrap();
    assert_eq!(logs.as_array().unwrap().len(), 2);
    assert_eq!(logs[0]["orb"], "counter");
    assert_eq!(logs[0]["message"], "mobile");
    assert_eq!(first.take_logs(), "[]");
    first.release(2).unwrap();
    first.release(1).unwrap();
    first.shutdown().unwrap();
    second.release(2).unwrap();
    second.release(1).unwrap();
    second.shutdown().unwrap();
    assert_eq!(backend.instances(), 0);
    assert!(first.begin(call().to_string(), false).is_err());
    let abandoned = backend
        .open(artifact, "{}".into(), "{}".into(), None)
        .unwrap();
    exchange(
        &abandoned,
        json!({"kind":"create","environment":1,"service":null}),
        true,
    );
    abandoned
        .begin(
            json!({"kind":"create","environment":2,"service":"counter"}).to_string(),
            true,
        )
        .unwrap();
    let request: Value = serde_json::from_str(&block_on(abandoned.receive()).unwrap()).unwrap();
    assert_eq!(request["event"], "host");
    assert_eq!(request["packet"], json!({"operation":"orb"}).to_string());
    // Register a foreign-style continuation while the VM still waits on its first host reply.
    // The next synchronous host request must wake it from an OS stack rather than a Wasmtime fiber.
    struct ForeignWake(std::sync::mpsc::Sender<String>);
    impl std::task::Wake for ForeignWake {
        fn wake(self: Arc<Self>) {
            let _ = self.0.send(
                std::thread::current()
                    .name()
                    .unwrap_or("unnamed")
                    .to_owned(),
            );
        }
    }
    let (wake, woke) = std::sync::mpsc::channel();
    let waker = std::task::Waker::from(Arc::new(ForeignWake(wake)));
    let mut context = std::task::Context::from_waker(&waker);
    let mut next = Box::pin(abandoned.receive());
    assert!(std::future::Future::poll(next.as_mut(), &mut context).is_pending());
    abandoned
        .reply(
            request["id"].as_u64().unwrap() as u32,
            json!({"ok":1}).to_string(),
        )
        .unwrap();
    assert_eq!(
        woke.recv_timeout(std::time::Duration::from_secs(2))
            .unwrap(),
        "tembra-mobile-events"
    );
    let second: Value = serde_json::from_str(&block_on(next).unwrap()).unwrap();
    // The factory's key read is still waiting on the host when the host abandons it.
    assert_eq!(second["event"], "host");
    assert_eq!(second["packet"], json!({"operation":"address"}).to_string());
    let start = std::time::Instant::now();
    abandoned.invalidate();
    abandoned.release(2).unwrap();
    abandoned.release(1).unwrap();
    assert!(start.elapsed() < std::time::Duration::from_secs(2));
    assert_eq!(backend.instances(), 0);
    assert!(block_on(abandoned.receive()).is_err());
}

#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn portable_source_io_obeys_read_only_mounts_and_network_destinations() {
    use std::io::{Read, Write};
    use std::time::{Duration, Instant};
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("input"), "mobile-file").unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (closed, observed) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        // The first peer answers in two writes, so the guest reads the response incrementally.
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        let mut byte = [0];
        while !request.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
        }
        assert_eq!(request, b"GET /mobile HTTP/1.0\r\n\r\n");
        stream.write_all(b"HTTP/1.0 200 OK\r\n\r\n").unwrap();
        std::thread::sleep(Duration::from_millis(50));
        stream.write_all(b"mobile").unwrap();
        drop(stream);
        // The second peer never answers; its socket must close when the callback completes.
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut rest = Vec::new();
        let _ = stream.read_to_end(&mut rest);
        let _ = closed.send(Instant::now());
    });
    let artifact = fixture("mobile-io");
    let grants = json!({"system":["clocks"],"filesystem":[{"provider":"data","mount":"/data","writable":false}],"network":[{"provider":"tcp","connect":[address.to_string()]}]});
    let installation: Value = serde_json::from_str(&artifact.request(json!({"kind":"install","name":"files","apiVersion":1,"requires":{},"grants":grants,"filesystem":["data"],"network":["tcp"]}).to_string())).unwrap();
    assert!(installation.get("error").is_none(), "{installation}");
    let backend = NativeInterpreter::new(
        json!({"filesystem":{"data":directory.path()},"network":["tcp"]}).to_string(),
    )
    .unwrap();
    let port = backend
        .open(
            artifact,
            installation["ok"]["bindings"].to_string(),
            grants.to_string(),
            None,
        )
        .unwrap();
    exchange(
        &port,
        json!({"kind":"create","environment":1,"service":null}),
        true,
    );
    exchange(
        &port,
        json!({"kind":"create","environment":2,"service":"files"}),
        true,
    );
    let call = |method: &str, args: Value| {
        exchange(
            &port,
            json!({"kind":"invoke","environment":2,"service":"files","method":method,"args":args}),
            false,
        )
    };
    assert_eq!(call("read", json!([])), "mobile-file");
    // Standard WASI 0.3 error codes: the read-only grant refuses writing, and no path leaves
    // the preopen.
    assert_eq!(call("denied", json!([])), "not-permitted");
    assert_eq!(call("escape", json!([])), "not-permitted");
    // A loopback listener outside the socket grant is refused before any connection is made.
    let ungranted = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    ungranted.set_nonblocking(true).unwrap();
    assert_eq!(
        call("connect", json!([ungranted.local_addr().unwrap().port()])),
        "access-denied"
    );
    assert_eq!(
        ungranted.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert_eq!(
        call("request", json!([address.port()])),
        "HTTP/1.0 200 OK\r\n\r\nmobile"
    );
    // Dropping an idle stream and its socket closes the connection promptly.
    let started = Instant::now();
    assert_eq!(call("abandon", json!([address.port()])), "cancelled|ok");
    let closed = observed.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(closed.duration_since(started) < Duration::from_secs(2));
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(
        std::fs::read_to_string(directory.path().join("input")).unwrap(),
        "mobile-file"
    );
    port.release(2).unwrap();
    port.release(1).unwrap();
    port.shutdown().unwrap();
    assert_eq!(backend.instances(), 0);
    server.join().unwrap();
}

/// Exercise source HTTP through the real mobile transport, including a body that never reaches EOF.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn portable_http_streams_through_the_host_provider() {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        time::Duration,
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (closed, observed) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        let mut byte = [0];
        while !request.ends_with(b"\r\n\r\n") {
            socket.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
        }
        assert!(request.starts_with(b"GET /events HTTP/1.1\r\n"));
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\nd\r\ndata: hello\n\n\r\n").unwrap();
        let mut rest = Vec::new();
        assert_eq!(socket.read_to_end(&mut rest).unwrap(), 0);
        closed.send(()).unwrap();
    });
    let artifact = fixture("mobile-web");
    let grants = json!({"http":[{"provider":"outbound","origins":[format!("http://{address}")]}]});
    let installation: Value = serde_json::from_str(&artifact.request(json!({"kind":"install","name":"web","apiVersion":1,"requires":{},"grants":grants,"filesystem":[],"network":["outbound"],"http":["outbound"]}).to_string())).unwrap();
    assert!(installation.get("error").is_none(), "{installation}");
    let backend = NativeInterpreter::new(json!({"network":["outbound"]}).to_string()).unwrap();
    let port = backend
        .open(
            artifact,
            installation["ok"]["bindings"].to_string(),
            grants.to_string(),
            None,
        )
        .unwrap();
    exchange(
        &port,
        json!({"kind":"create","environment":1,"service":null}),
        true,
    );
    exchange(
        &port,
        json!({"kind":"create","environment":2,"service":"web"}),
        true,
    );
    let value = exchange(
        &port,
        json!({"kind":"invoke","environment":2,"service":"web","method":"first","args":[format!("http://{address}/events")]}),
        false,
    );
    // Status and head are visible; the body stops after the first event of an endless stream.
    assert_eq!(
        value,
        json!({"tag":"ok","val":"200|text/event-stream|data: hello\n\n"})
    );
    // Closing the idle SSE response closed the connection before the callback completed,
    // while the Orb is still installed.
    observed.recv_timeout(Duration::from_secs(5)).unwrap();
    port.release(2).unwrap();
    port.release(1).unwrap();
    port.shutdown().unwrap();
    assert_eq!(backend.instances(), 0);
    server.join().unwrap();
}

/// A cancelled caller revokes its callback's pending standard HTTP send (native, in-Store: no
/// host packet is involved); the connection closes and the Store keeps serving.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn portable_cancel_interrupts_a_pending_http_send_without_quarantine() {
    use std::{
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        sync::mpsc,
        time::{Duration, Instant},
    };
    fn head(socket: &mut TcpStream) -> Vec<u8> {
        let mut request = Vec::new();
        let mut byte = [0];
        while !request.ends_with(b"\r\n\r\n") {
            socket.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
        }
        request
    }
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (entered, waiting) = mpsc::channel();
    let (closed, observed) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut stalled, _) = listener.accept().unwrap();
        stalled
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        assert!(head(&mut stalled).starts_with(b"GET /stalled HTTP/1.1\r\n"));
        entered.send(()).unwrap();
        // Never answer: only the revoked send can close this connection before the deadline.
        let _ = stalled.read_to_end(&mut Vec::new());
        closed.send(Instant::now()).unwrap();
        let (mut answered, _) = listener.accept().unwrap();
        assert!(head(&mut answered).starts_with(b"GET /values HTTP/1.1\r\n"));
        answered
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello")
            .unwrap();
    });
    let artifact = fixture("mobile-web");
    let grants = json!({"http":[{"provider":"outbound","origins":[format!("http://{address}")]}]});
    let installation: Value = serde_json::from_str(&artifact.request(json!({"kind":"install","name":"web","apiVersion":1,"requires":{},"grants":grants,"filesystem":[],"network":["outbound"],"http":["outbound"]}).to_string())).unwrap();
    assert!(installation.get("error").is_none(), "{installation}");
    let backend = NativeInterpreter::new(json!({"network":["outbound"]}).to_string()).unwrap();
    let port = backend
        .open(
            artifact,
            installation["ok"]["bindings"].to_string(),
            grants.to_string(),
            None,
        )
        .unwrap();
    exchange(
        &port,
        json!({"kind":"create","environment":1,"service":null}),
        true,
    );
    exchange(
        &port,
        json!({"kind":"create","environment":2,"service":"web"}),
        true,
    );
    let invoke = |path: &str| json!({"kind":"invoke","environment":2,"service":"web","method":"first","args":[format!("http://{address}/{path}")]});

    // The caller aborts once the peer holds the request head: standard HTTP is native, so the
    // cancellation comes from another thread rather than from a host operation event.
    port.begin(invoke("stalled").to_string(), false).unwrap();
    let canceller = {
        let port = port.clone();
        std::thread::spawn(move || {
            waiting.recv_timeout(Duration::from_secs(5)).unwrap();
            std::thread::sleep(Duration::from_millis(100));
            let cancelled = Instant::now();
            port.cancel().unwrap();
            cancelled
        })
    };
    // Standard HTTP is native: besides the SDK's synchronous admission check before sending,
    // the only event is the callback's completion.
    let complete = loop {
        let event: Value = serde_json::from_str(&block_on(port.receive()).unwrap()).unwrap();
        if event["event"] == "complete" {
            break event;
        }
        assert_eq!(
            event["packet"],
            json!({"operation":"context.check"}).to_string(),
            "unexpected host operation {event}"
        );
        port.reply(
            event["id"].as_u64().unwrap() as u32,
            json!({"ok":null}).to_string(),
        )
        .unwrap();
    };
    let cancelled = canceller.join().unwrap();
    port.finish().unwrap();
    let settled = cancelled.elapsed();
    // The revoked send settles with a standard error; the callback reports it through its
    // declared WIT `result` lane, so it is a recoverable business error rather than a trap.
    let returned: Value =
        serde_json::from_str(complete["result"]["ok"].as_str().unwrap_or_else(|| {
            panic!("the revoked send must complete through the result lane: {complete}")
        }))
        .unwrap();
    assert_eq!(returned["tag"], "err", "{complete}");
    assert!(
        returned["val"]
            .as_str()
            .unwrap()
            .starts_with("HTTP operation failed"),
        "{complete}"
    );
    assert!(
        settled < Duration::from_secs(2),
        "settled {settled:?} after cancel"
    );
    let closed = observed.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(closed.duration_since(cancelled) < Duration::from_secs(2));

    // A stale cancellation outside any exchange does nothing, and the Store is not quarantined.
    port.cancel().unwrap();
    let started = Instant::now();
    assert_eq!(
        exchange(&port, invoke("values"), false),
        json!({"tag":"ok","val":"200||hello"})
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    port.release(2).unwrap();
    port.release(1).unwrap();
    port.shutdown().unwrap();
    assert_eq!(backend.instances(), 0);
    server.join().unwrap();
}

/// Without a declared failure lane, a revoked standard send is an uncaught exception of a
/// non-`result` export: by the component semantics it traps, so the revision fails closed. The
/// connection still closes, later calls are refused and cleanup leaves no Store behind.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn portable_cancel_of_a_strict_callback_fails_closed() {
    use std::{
        io::Read,
        net::TcpListener,
        sync::mpsc,
        time::{Duration, Instant},
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (entered, waiting) = mpsc::channel();
    let (closed, observed) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut stalled, _) = listener.accept().unwrap();
        stalled
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        let mut request = Vec::new();
        let mut byte = [0];
        while !request.ends_with(b"\r\n\r\n") {
            stalled.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
        }
        assert!(request.starts_with(b"GET /stalled HTTP/1.1\r\n"));
        entered.send(()).unwrap();
        let _ = stalled.read_to_end(&mut Vec::new());
        closed.send(Instant::now()).unwrap();
    });
    let artifact = fixture("mobile-web");
    let grants = json!({"http":[{"provider":"outbound","origins":[format!("http://{address}")]}]});
    let installation: Value = serde_json::from_str(&artifact.request(json!({"kind":"install","name":"web","apiVersion":1,"requires":{},"grants":grants,"filesystem":[],"network":["outbound"],"http":["outbound"]}).to_string())).unwrap();
    assert!(installation.get("error").is_none(), "{installation}");
    let backend = NativeInterpreter::new(json!({"network":["outbound"]}).to_string()).unwrap();
    let port = backend
        .open(
            artifact,
            installation["ok"]["bindings"].to_string(),
            grants.to_string(),
            None,
        )
        .unwrap();
    exchange(
        &port,
        json!({"kind":"create","environment":1,"service":null}),
        true,
    );
    exchange(
        &port,
        json!({"kind":"create","environment":2,"service":"web"}),
        true,
    );
    let invoke = |path: &str| json!({"kind":"invoke","environment":2,"service":"web","method":"strict","args":[format!("http://{address}/{path}")]});
    port.begin(invoke("stalled").to_string(), false).unwrap();
    let canceller = {
        let port = port.clone();
        std::thread::spawn(move || {
            waiting.recv_timeout(Duration::from_secs(5)).unwrap();
            std::thread::sleep(Duration::from_millis(100));
            let cancelled = Instant::now();
            port.cancel().unwrap();
            cancelled
        })
    };
    let complete = loop {
        let event: Value = serde_json::from_str(&block_on(port.receive()).unwrap()).unwrap();
        if event["event"] == "complete" {
            break event;
        }
        assert_eq!(
            event["packet"],
            json!({"operation":"context.check"}).to_string(),
            "unexpected host operation {event}"
        );
        port.reply(
            event["id"].as_u64().unwrap() as u32,
            json!({"ok":null}).to_string(),
        )
        .unwrap();
    };
    let cancelled = canceller.join().unwrap();
    port.finish().unwrap();
    assert!(cancelled.elapsed() < Duration::from_secs(2));
    // The uncaught failure is a trap, reported as an error, never as a value.
    assert!(complete["result"].get("ok").is_none(), "{complete}");
    let message = complete["result"]["error"]["message"].as_str().unwrap();
    assert!(message.contains("component trap"), "{complete}");
    let closed = observed.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(closed.duration_since(cancelled) < Duration::from_secs(2));
    // Fail closed: the trapped revision admits no further callback.
    let refused = port.begin(invoke("values").to_string(), false).unwrap_err();
    assert!(
        format!("{refused:?}").contains("quarantined"),
        "{refused:?}"
    );
    port.release(2).unwrap();
    port.release(1).unwrap();
    port.shutdown().unwrap();
    assert_eq!(backend.instances(), 0);
    server.join().unwrap();
}

#[test]
#[ignore = "requires TEMBRA_WASM_PROVIDER"]
fn portable_compiled_orb_calls_the_same_wit_abi() {
    let raw = std::fs::read(std::env::var_os("TEMBRA_WASM_PROVIDER").unwrap()).unwrap();
    let contract = Contract::from_source(
        "arithmetic.wit",
        "package example:integrated@1.0.0; interface arithmetic { double: func(value: s32) -> s32; } world arithmetic-provider { export arithmetic; }",
        Some("arithmetic-provider"),
    )
    .unwrap();
    let artifact = NativeArtifact::new(
        contract
            .component_artifact(&raw, "arithmetic", 1, &Default::default())
            .unwrap(),
    )
    .unwrap();
    let backend = NativeInterpreter::new("{}".into()).unwrap();
    let port = backend
        .open(artifact, "{}".into(), "{}".into(), None)
        .unwrap();
    exchange(
        &port,
        json!({"kind":"create","environment":1,"service":"arithmetic"}),
        true,
    );
    let result = exchange(
        &port,
        json!({"kind":"invoke","environment":1,"service":"arithmetic","method":"double","args":[21]}),
        false,
    );
    assert_eq!(result, json!(42));
    port.release(1).unwrap();
    port.shutdown().unwrap();
    assert_eq!(backend.instances(), 0);
}

/// A compiled `wasi:http/handler` streams through the typed UniFFI carrier: raw-byte head and
/// fields, one request chunk per pull, one response frame per pull, trailers, then completion.
#[test]
#[ignore = "requires TEMBRA_WASM_HTTP_HANDLER"]
fn portable_compiled_handler_streams_through_the_typed_carrier() {
    use tembra_mobile::{MobileField, MobileIngressEvent, MobileRequestChunk, MobileRequestHead};
    let raw = std::fs::read(std::env::var_os("TEMBRA_WASM_HTTP_HANDLER").unwrap()).unwrap();
    let contract = Contract::from_dir(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../executor-wasm/tests/wit/handler"
        ),
        Some("handler"),
    )
    .unwrap();
    let artifact = NativeArtifact::new(
        contract
            .component_artifact(&raw, "web", 1, &Default::default())
            .unwrap(),
    )
    .unwrap();
    let bindings =
        json!({"wasi:http/types@0.3.0": {"kind": "host", "provider": "wasi:http/handler@0.3.0"}});
    let backend = NativeInterpreter::new("{}".into()).unwrap();
    let port = backend
        .open(artifact, bindings.to_string(), "{}".into(), None)
        .unwrap();
    exchange(
        &port,
        json!({"kind":"create","environment":1,"service":null}),
        true,
    );
    exchange(
        &port,
        json!({"kind":"create","environment":2,"service":"handler"}),
        true,
    );
    port.begin_ingress(
        json!({"kind":"ingress","environment":2,"service":"handler","method":"handle"}).to_string(),
        MobileRequestHead {
            method: "POST".into(),
            scheme: Some("http".into()),
            authority: Some("ingress.test".into()),
            path_with_query: Some("/sse".into()),
            fields: vec![MobileField {
                name: "x-raw".into(),
                value: vec![0xff, b'A', 0x80],
            }],
        },
    )
    .unwrap();
    let mut chunks = vec![
        MobileRequestChunk::Data {
            bytes: b"alpha".to_vec(),
        },
        MobileRequestChunk::End { trailers: vec![] },
    ]
    .into_iter();
    let mut frames = Vec::new();
    let result = loop {
        match block_on(port.receive_ingress()).unwrap() {
            MobileIngressEvent::Head { status, fields } => {
                assert_eq!(status, 200);
                assert!(fields.iter().any(
                    |field| field.name == "content-type" && field.value == b"text/event-stream"
                ));
                cancel_receive_before_next_poll(&port, &mut chunks);
            }
            MobileIngressEvent::RequestPull => {
                assert!(port.supply_request(chunks.next().unwrap()).unwrap());
            }
            MobileIngressEvent::Data { bytes } => {
                frames.push(String::from_utf8(bytes).unwrap());
                port.pull_response().unwrap();
            }
            MobileIngressEvent::Trailers { fields } => {
                assert_eq!(fields.len(), 1);
                assert_eq!(fields[0].value, b"3");
                port.pull_response().unwrap();
            }
            MobileIngressEvent::End => {}
            MobileIngressEvent::Error { message } => panic!("response failed: {message}"),
            MobileIngressEvent::Host { packet, .. } => panic!("unexpected host request {packet}"),
            MobileIngressEvent::Refused { .. } => panic!("a response is not a refusal"),
            MobileIngressEvent::Complete { result } => break result,
        }
    };
    port.finish().unwrap();
    assert_eq!(result, r#"{"ok":"null"}"#);
    assert_eq!(
        frames,
        ["data: open\n\n", "data: alpha\n\n", "data: end\n\n"]
    );
    // A handler `error-code` arrives typed (standard codec bytes) before the completion, with
    // no head: `http-response-body-size(some(u64::MAX))`.
    port.begin_ingress(
        json!({"kind":"ingress","environment":2,"service":"handler","method":"handle"}).to_string(),
        MobileRequestHead {
            method: "GET".into(),
            scheme: None,
            authority: None,
            path_with_query: Some("/refuse/u64".into()),
            fields: vec![],
        },
    )
    .unwrap();
    let mut refused = None;
    loop {
        match block_on(port.receive_ingress()).unwrap() {
            MobileIngressEvent::Refused { code } => {
                assert!(refused.replace(code).is_none(), "one refusal");
            }
            MobileIngressEvent::Complete { result } => {
                assert!(result.contains("\"error\""), "{result}");
                break;
            }
            MobileIngressEvent::RequestPull => {
                port.supply_request(MobileRequestChunk::End { trailers: vec![] })
                    .unwrap();
            }
            _ => panic!("a refusal has no head or response frames"),
        }
    }
    port.finish().unwrap();
    let mut expected = 28u32.to_le_bytes().to_vec();
    expected.push(1);
    expected.extend_from_slice(&u64::MAX.to_le_bytes());
    assert_eq!(refused.expect("refused before complete"), expected);
    port.release(2).unwrap();
    port.release(1).unwrap();
    port.shutdown().unwrap();
    assert_eq!(backend.instances(), 0);
}

/// The foreign future is cancelled after its wake, before it polls the queued event. A
/// detached consuming task would lose that event; the following receive must still see it.
fn cancel_receive_before_next_poll(
    port: &Arc<NativePort>,
    chunks: &mut impl Iterator<Item = tembra_mobile::MobileRequestChunk>,
) {
    use std::{
        future::Future,
        task::{Context, Poll, Wake, Waker},
        time::Duration,
    };
    use tembra_mobile::MobileIngressEvent;

    struct RecordWake(std::sync::mpsc::SyncSender<String>);
    impl Wake for RecordWake {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            let _ = self
                .0
                .try_send(std::thread::current().name().unwrap_or("").into());
        }
    }

    let (send, woke) = std::sync::mpsc::sync_channel(1);
    let waker = Waker::from(Arc::new(RecordWake(send)));
    let mut context = Context::from_waker(&waker);
    let receive = loop {
        let mut receive = Box::pin(port.receive_ingress());
        match receive.as_mut().poll(&mut context) {
            Poll::Pending => break receive,
            Poll::Ready(Ok(MobileIngressEvent::RequestPull)) => {
                assert!(port.supply_request(chunks.next().unwrap()).unwrap());
            }
            _ => panic!("unexpected carrier event before the first response pull"),
        }
    };
    port.pull_response().unwrap();
    assert_eq!(
        woke.recv_timeout(Duration::from_secs(2)).unwrap(),
        "tembra-mobile-events",
        "foreign continuations must wake on the delivery runtime's OS thread"
    );
    drop(receive);
}
