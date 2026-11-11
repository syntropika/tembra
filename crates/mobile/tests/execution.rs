//! Exercise the actual mobile transport and portable Wasmtime engine, without mocked guest execution.
use futures::executor::block_on;
use serde_json::{Value, json};
use std::sync::Arc;
use tembra_contracts::{Artifact, Contract};
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
            "orb" => Value::Null,
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

#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE"]
fn portable_source_transport_preserves_factories_and_isolates_heaps() {
    let engine = std::fs::read(std::env::var_os("TEMBRA_WASM_ENGINE").unwrap()).unwrap();
    let contract = Contract::from_source("mobile.wit", "package test:mobile; interface counter { add: func() -> s32; } world app { export counter; }", Some("app")).unwrap();
    let bytes = Artifact::package_source("counter", 1, contract.prepared(), "globalThis.__tembraOrb = { services: { counter: { create(self) { self.key; self.key; return { value: 0 }; }, methods: { add(self) { console.log(\"mobile\"); return ++self.value; } } } } };").unwrap();
    let artifact = NativeArtifact::new(bytes).unwrap();
    let backend = NativeInterpreter::new("{}".into(), engine).unwrap();
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
    let add = contract.bind("test:mobile/counter", "add", true).unwrap();
    let call = || json!({"kind":"invoke","environment":2,"function":add,"args":[]});
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
            json!({"ok":{"key":[],"principal":"test"}}).to_string(),
        )
        .unwrap();
    assert_eq!(
        woke.recv_timeout(std::time::Duration::from_secs(2))
            .unwrap(),
        "tembra-mobile-events"
    );
    let second: Value = serde_json::from_str(&block_on(next).unwrap()).unwrap();
    assert_eq!(second["event"], "host");
    let start = std::time::Instant::now();
    abandoned.invalidate();
    abandoned.release(2).unwrap();
    abandoned.release(1).unwrap();
    assert!(start.elapsed() < std::time::Duration::from_secs(2));
    assert_eq!(backend.instances(), 0);
    assert!(block_on(abandoned.receive()).is_err());
}

#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE"]
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
    let contract = Contract::from_source("mobile.wit", "package test:mobile; interface files { read: async func() -> string; denied: async func() -> bool; escape: async func() -> bool; request: async func(port: u16) -> string; abandon: async func(port: u16) -> bool; } world app { import wasi:filesystem/types@0.3.0; import wasi:filesystem/preopens@0.3.0; import wasi:sockets/types@0.3.0; export files; }", Some("app")).unwrap();
    let source = r#"const preopens = __tembraStandard['wasi:filesystem/preopens@0.3.0'];
    const mount = (path) => { const [[root]] = preopens.getDirectories(); if (!path.startsWith('/data/')) throw new Error('outside the granted mount'); return [root, path.slice(6)]; };
    const readText = async (path) => { const [root, relative] = mount(path); const fd = await root.openAt({}, relative, {}, {read: true}); const [stream, done] = fd.readViaStream(0n); let text = ''; for await (const byte of stream) text += String.fromCharCode(byte); await done; return text; };
    const ascii = (text) => Uint8Array.from(text, (character) => character.charCodeAt(0));
    const sockets = __tembraStandard['wasi:sockets/types@0.3.0'];
    const dispose = Symbol.dispose ?? Symbol.for('Symbol.dispose');
    const connect = async (port) => {
        const socket = sockets.TcpSocket.create('ipv4');
        await socket.connect({ tag: 'ipv4', val: { port, address: [127, 0, 0, 1] } });
        return socket;
    };
    globalThis.__tembraOrb = { services: { files: { methods: {
        async read() { return await readText('/data/input'); },
        async denied() { try { const [root, relative] = mount('/data/input'); await root.openAt({}, relative, {truncate: true}, {write: true}); return false; } catch { return true; } },
        async escape() { try { const [[root]] = preopens.getDirectories(); await root.openAt({}, '../secret', {}, {read: true}); return false; } catch { return true; } },
        async request(self, context, port) {
            const socket = await connect(port);
            const sent = socket.send(ascii('GET /mobile HTTP/1.0\r\n\r\n'));
            const [stream, done] = socket.receive();
            let text = '';
            for await (const byte of stream) text += String.fromCharCode(byte);
            await sent;
            await done;
            return text;
        },
        async abandon(self, context, port) {
            // A read is pending when the guest drops its stream and socket: dropping the last
            // references closes the connection although the peer never sent anything.
            const socket = await connect(port);
            const [stream, done] = socket.receive();
            stream[Symbol.asyncIterator]().next().catch(() => {});
            stream[dispose]?.();
            done[dispose]?.();
            socket[dispose]?.();
            return true;
        }
    } } } };"#;
    let artifact = NativeArtifact::new(
        Artifact::package_source("files", 1, contract.prepared(), source).unwrap(),
    )
    .unwrap();
    let grants = json!({"system":["clocks"],"filesystem":[{"provider":"data","mount":"/data","writable":false}],"network":[{"provider":"tcp","connect":[address.to_string()]}]});
    let installation: Value = serde_json::from_str(&artifact.request(json!({"kind":"install","name":"files","apiVersion":1,"requires":{},"grants":grants,"filesystem":["data"],"network":["tcp"]}).to_string())).unwrap();
    assert!(installation.get("error").is_none(), "{installation}");
    let engine = std::fs::read(std::env::var_os("TEMBRA_WASM_ENGINE").unwrap()).unwrap();
    let backend = NativeInterpreter::new(
        json!({"filesystem":{"data":directory.path()},"network":["tcp"]}).to_string(),
        engine,
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
        let function = contract.bind("test:mobile/files", method, true).unwrap();
        exchange(
            &port,
            json!({"kind":"invoke","environment":2,"function":function,"args":args}),
            false,
        )
    };
    assert_eq!(call("read", json!([])), "mobile-file");
    assert_eq!(call("denied", json!([])), true);
    assert_eq!(call("escape", json!([])), true);
    assert_eq!(
        call("request", json!([address.port()])),
        "HTTP/1.0 200 OK\r\n\r\nmobile"
    );
    // Dropping an idle stream and its socket closes the connection promptly.
    let started = Instant::now();
    assert_eq!(call("abandon", json!([address.port()])), true);
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
#[ignore = "requires TEMBRA_WASM_ENGINE"]
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
    let contract = Contract::from_source("http.wit", "package test:mobile; interface web { first: async func(url: string) -> string; } world app { import wasi:http/types@0.3.0; import wasi:http/client@0.3.0; export web; }", Some("app")).unwrap();
    let source = r#"globalThis.__tembraOrb = { services: { web: { methods: {
        async first(_self, _context, url) {
            // Standard wasi:http only: build the request, send it, read the first event or
            // the whole short body, then drop the stream.
            const types = __tembraStandard['wasi:http/types@0.3.0'];
            const client = __tembraStandard['wasi:http/client@0.3.0'];
            const [, , authority, ...path] = url.split('/');
            const done = Promise.resolve({ tag: 'ok', val: undefined });
            const [request] = types.Request.new(new types.Fields(), undefined, done, undefined);
            request.setScheme({ tag: 'HTTP' });
            request.setAuthority(authority);
            request.setPathWithQuery('/' + path.join('/'));
            const response = await client.send(request);
            // The response-handling future: resolved `err` on close, like the SDK's close().
            let finish;
            const handled = new Promise((resolve) => { finish = resolve; });
            const [body, trailers] = types.Response.consumeBody(response, handled);
            const bytes = [];
            for await (const byte of body) {
                bytes.push(byte);
                if (bytes.length > 1 && bytes[bytes.length - 1] === 10 && bytes[bytes.length - 2] === 10) break;
            }
            // Release the whole response, like the SDK's close(): the stream (ended by the
            // loop) and the trailers future, so the host may close the connection now.
            const dispose = Symbol.dispose ?? Symbol.for('Symbol.dispose');
            body[dispose]?.();
            trailers[dispose]?.();
            finish({ tag: 'err', val: { tag: 'internal-error', val: 'response closed' } });
            return String.fromCharCode(...bytes);
        }
    } } } };"#;
    let artifact = NativeArtifact::new(
        Artifact::package_source("web", 1, contract.prepared(), source).unwrap(),
    )
    .unwrap();
    let grants = json!({"http":[{"provider":"outbound","origins":[format!("http://{address}")]}]});
    let installation: Value = serde_json::from_str(&artifact.request(json!({"kind":"install","name":"web","apiVersion":1,"requires":{},"grants":grants,"filesystem":[],"network":["outbound"],"http":["outbound"]}).to_string())).unwrap();
    assert!(installation.get("error").is_none(), "{installation}");
    let engine = std::fs::read(std::env::var_os("TEMBRA_WASM_ENGINE").unwrap()).unwrap();
    let backend =
        NativeInterpreter::new(json!({"network":["outbound"]}).to_string(), engine).unwrap();
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
    let first = contract.bind("test:mobile/web", "first", true).unwrap();
    let value = exchange(
        &port,
        json!({"kind":"invoke","environment":2,"function":first,"args":[format!("http://{address}/events")]}),
        false,
    );
    assert_eq!(value, "data: hello\n\n");
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
#[ignore = "requires TEMBRA_WASM_ENGINE"]
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
    let contract = Contract::from_source("http.wit", "package test:mobile; interface web { first: async func(url: string) -> string; } world app { import wasi:http/types@0.3.0; import wasi:http/client@0.3.0; export web; }", Some("app")).unwrap();
    let source = r#"globalThis.__tembraOrb = { services: { web: { methods: {
        async first(_self, _context, url) {
            // Standard wasi:http only: build the request, send it, read the first event or
            // the whole short body, then drop the stream.
            const types = __tembraStandard['wasi:http/types@0.3.0'];
            const client = __tembraStandard['wasi:http/client@0.3.0'];
            const [, , authority, ...path] = url.split('/');
            const done = Promise.resolve({ tag: 'ok', val: undefined });
            const [request] = types.Request.new(new types.Fields(), undefined, done, undefined);
            request.setScheme({ tag: 'HTTP' });
            request.setAuthority(authority);
            request.setPathWithQuery('/' + path.join('/'));
            const response = await client.send(request);
            // The response-handling future: resolved `err` on close, like the SDK's close().
            let finish;
            const handled = new Promise((resolve) => { finish = resolve; });
            const [body, trailers] = types.Response.consumeBody(response, handled);
            const bytes = [];
            for await (const byte of body) {
                bytes.push(byte);
                if (bytes.length > 1 && bytes[bytes.length - 1] === 10 && bytes[bytes.length - 2] === 10) break;
            }
            // Release the whole response, like the SDK's close(): the stream (ended by the
            // loop) and the trailers future, so the host may close the connection now.
            const dispose = Symbol.dispose ?? Symbol.for('Symbol.dispose');
            body[dispose]?.();
            trailers[dispose]?.();
            finish({ tag: 'err', val: { tag: 'internal-error', val: 'response closed' } });
            return String.fromCharCode(...bytes);
        }
    } } } };"#;
    let artifact = NativeArtifact::new(
        Artifact::package_source("web", 1, contract.prepared(), source).unwrap(),
    )
    .unwrap();
    let grants = json!({"http":[{"provider":"outbound","origins":[format!("http://{address}")]}]});
    let installation: Value = serde_json::from_str(&artifact.request(json!({"kind":"install","name":"web","apiVersion":1,"requires":{},"grants":grants,"filesystem":[],"network":["outbound"],"http":["outbound"]}).to_string())).unwrap();
    assert!(installation.get("error").is_none(), "{installation}");
    let engine = std::fs::read(std::env::var_os("TEMBRA_WASM_ENGINE").unwrap()).unwrap();
    let backend =
        NativeInterpreter::new(json!({"network":["outbound"]}).to_string(), engine).unwrap();
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
    let first = contract.bind("test:mobile/web", "first", true).unwrap();
    let invoke = |path: &str| json!({"kind":"invoke","environment":2,"function":first,"args":[format!("http://{address}/{path}")]});

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
    // Standard HTTP is native: the only event is the callback's completion.
    let complete: Value = serde_json::from_str(&block_on(port.receive()).unwrap()).unwrap();
    assert_eq!(
        complete["event"], "complete",
        "unexpected host operation {complete}"
    );
    let cancelled = canceller.join().unwrap();
    port.finish().unwrap();
    let settled = cancelled.elapsed();
    assert!(
        complete["result"].get("error").is_some(),
        "the revoked send must fail its callback: {complete}"
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
    assert_eq!(exchange(&port, invoke("values"), false), "hello");
    assert!(started.elapsed() < Duration::from_secs(5));
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
    let backend = NativeInterpreter::new("{}".into(), vec![]).unwrap();
    let port = backend
        .open(artifact, "{}".into(), "{}".into(), None)
        .unwrap();
    exchange(
        &port,
        json!({"kind":"create","environment":1,"service":"arithmetic"}),
        true,
    );
    let double = contract
        .bind("example:integrated/arithmetic@1.0.0", "double", true)
        .unwrap();
    let result = exchange(
        &port,
        json!({"kind":"invoke","environment":1,"function":double,"args":[21]}),
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
    let backend = NativeInterpreter::new("{}".into(), vec![]).unwrap();
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
