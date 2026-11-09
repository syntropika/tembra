//! Exercise the actual mobile transport and portable Wasmtime engine, without mocked guest execution.
use futures::executor::block_on;
use serde_json::{Value, json};
use std::sync::Arc;
use tembra_contracts::{Artifact, Contract};
use tembra_mobile::{NativeArtifact, NativeInterpreter, NativePort};

/// Drive one exchange like the SDK mobile driver: host I/O runs concurrently with event
/// delivery, and the ticket finishes only after every admitted operation has settled.
fn exchange(port: &Arc<NativePort>, command: Value, declaration: bool) -> Value {
    port.begin(command.to_string(), declaration).unwrap();
    let mut pending = Vec::new();
    loop {
        let event: Value = serde_json::from_str(&block_on(port.receive()).unwrap()).unwrap();
        if event["event"] == "complete" {
            for operation in pending {
                std::thread::JoinHandle::join(operation).unwrap();
            }
            port.finish().unwrap();
            assert!(
                event["result"].get("error").is_none(),
                "command {command}: {event}"
            );
            return serde_json::from_str(event["result"]["ok"].as_str().unwrap()).unwrap();
        }
        let packet: Value = serde_json::from_str(event["packet"].as_str().unwrap()).unwrap();
        if event["asynchronous"] == true {
            let port = port.clone();
            let id = event["id"].as_u64().unwrap() as u32;
            pending.push(std::thread::spawn(move || {
                let response = match block_on(port.io(packet.to_string())) {
                    Ok(value) => format!("{{\"ok\":{value}}}"),
                    Err(error) => json!({"error":error.to_string()}).to_string(),
                };
                // A reply after completion is ignored, as for a late platform observer.
                let _ = port.reply(id, response);
            }));
            continue;
        }
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
        .open(artifact.clone(), "{}".into(), "{}".into())
        .unwrap();
    let second = backend
        .open(artifact.clone(), "{}".into(), "{}".into())
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
    let abandoned = backend.open(artifact, "{}".into(), "{}".into()).unwrap();
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
    let contract = Contract::from_source("mobile.wit", "package test:mobile; interface files { read: async func() -> string; denied: async func() -> bool; escape: async func() -> bool; request: async func(port: u16) -> string; abandon: async func(port: u16) -> bool; } world app { import wasi:filesystem/preopens@0.3.0; import wasi:sockets/types@0.3.0; export files; }", Some("app")).unwrap();
    let source = r#"const ascii = (text) => Uint8Array.from(text, (character) => character.charCodeAt(0));
    globalThis.__tembraOrb = { services: { files: { methods: {
        async read() { return await tembra.filesystem.readText('/data/input'); },
        async denied() { try { await tembra.filesystem.writeText('/data/input', 'bad'); return false; } catch { return true; } },
        async escape() { try { await tembra.filesystem.readText('/data/../secret'); return false; } catch { return true; } },
        async request(self, context, port) {
            const stream = await tembra.transport.connect('127.0.0.1', port);
            await stream.write(ascii('GET /mobile HTTP/1.0\r\n\r\n'));
            let text = '';
            for (let chunk = await stream.read(); chunk !== null; chunk = await stream.read())
                text += String.fromCharCode(...chunk);
            await stream.close();
            return text;
        },
        async abandon(self, context, port) {
            const stream = await tembra.transport.connect('127.0.0.1', port);
            stream.read().catch(() => {});
            return true;
        }
    } } } };"#;
    let artifact = NativeArtifact::new(
        Artifact::package_source("files", 1, contract.prepared(), source).unwrap(),
    )
    .unwrap();
    let grants = json!({"filesystem":[{"provider":"data","mount":"/data","writable":false}],"network":[{"provider":"tcp","connect":[address.to_string()]}]});
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
    assert_eq!(call("denied", json!([])), true);
    assert_eq!(call("escape", json!([])), true);
    assert_eq!(
        call("request", json!([address.port()])),
        "HTTP/1.0 200 OK\r\n\r\nmobile"
    );
    // An abandoned read settles when its callback completes, well before its I/O deadline.
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
    let port = backend.open(artifact, "{}".into(), "{}".into()).unwrap();
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
