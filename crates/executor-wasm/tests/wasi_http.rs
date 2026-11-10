//! A compiled component sends standard `wasi:http` requests through the host's WASI HTTP stack.
//!
//! The guest (`rust-http-client`) uses upstream wit-bindgen resources: `request`, `client.send`,
//! `response.consume-body` and its byte stream. Peers write raw HTTP/1.1 bytes so each test
//! controls framing and timing.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    rc::Rc,
    sync::mpsc,
    thread,
    time::Duration,
};
use tembra::Host;
use tembra_contracts::HttpGrant;
use tembra_executor_wasm::{Contract, Engine, Grants, Install, Limits, NetworkGrant};

const SERVICE: &str = "web::example:fetch/fetcher@1.0.0";

fn component() -> Option<Vec<u8>> {
    let Some(path) = std::env::var_os("TEMBRA_WASM_HTTP_CLIENT") else {
        eprintln!("skipped: TEMBRA_WASM_HTTP_CLIENT names no compiled HTTP client");
        return None;
    };
    Some(std::fs::read(path).unwrap())
}

fn artifact(component: &[u8]) -> Vec<u8> {
    Contract::from_dir(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/wit/http"),
        Some("client"),
    )
    .unwrap()
    .component_artifact(component, "web", 1, &Default::default())
    .unwrap()
}

fn install(origins: &[String]) -> Install {
    let mut install = Install::default();
    install.grants = Grants {
        http: vec![HttpGrant {
            provider: "web".into(),
            origins: origins.to_vec(),
        }],
        ..Grants::default()
    };
    install
}

/// Read a request head from a peer connection.
fn request(socket: &mut TcpStream) -> String {
    let mut head = Vec::new();
    let mut byte = [0];
    while !head.ends_with(b"\r\n\r\n") {
        socket.read_exact(&mut byte).unwrap();
        head.push(byte[0]);
    }
    String::from_utf8(head).unwrap()
}

/// Serve one scripted connection; report when the host side closes it.
fn peer(script: impl FnOnce(&mut TcpStream) + Send + 'static) -> (SocketAddr, mpsc::Receiver<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (closed, observed) = mpsc::channel();
    thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        script(&mut socket);
        let mut rest = Vec::new();
        let _ = socket.read_to_end(&mut rest);
        let _ = closed.send(());
    });
    (address, observed)
}

/// Call one fetcher method under a fresh root.
fn call(engine: &Engine, install: Install, method: &'static str, args: Value) -> Value {
    call_then(engine, install, method, args, || {})
}

/// Call one fetcher method, then run `after` while the Orb and its Store are still installed.
fn call_then(
    engine: &Engine,
    install: Install,
    method: &'static str,
    args: Value,
    after: impl FnOnce(),
) -> Value {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        engine
            .load(&artifact(&component().unwrap()), install)
            .unwrap(),
    ])
    .unwrap();
    let reference = Rc::new(host.get(SERVICE, vec!["main".into()], "alice").unwrap());
    pool.run_until(reference.ready()).unwrap();
    let called = reference.clone();
    let result = pool
        .run_until(host.root("alice", move |context| {
            async move { called.call_value(&context, method, args).await }.boxed_local()
        }))
        .unwrap();
    after();
    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
    result
}

fn engines() -> [Engine; 2] {
    [
        Engine::new(Limits::default())
            .unwrap()
            .network("web")
            .unwrap(),
        Engine::interpreted(Limits::default())
            .unwrap()
            .network("web")
            .unwrap(),
    ]
}

/// Chunked bodies, incremental reads, early drops and origin checks through standard resources.
#[test]
#[ignore = "requires TEMBRA_WASM_HTTP_CLIENT"]
fn compiled_components_use_standard_wasi_http() {
    if component().is_none() {
        return;
    }
    for engine in engines() {
        let (address, closed) = peer(|socket| {
            let head = request(socket);
            assert!(head.starts_with("GET /chunked HTTP/1.1\r\n"), "{head}");
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n")
                .unwrap();
        });
        let origin = format!("http://{address}");
        let result = call(
            &engine,
            install(std::slice::from_ref(&origin)),
            "fetch",
            json!(["http", address.to_string(), "/chunked"]),
        );
        assert_eq!(result, json!({"tag": "ok", "val": "200 hello world"}));
        closed.recv_timeout(Duration::from_secs(5)).unwrap();

        // The first event is delivered while the peer keeps its stream open, and dropping the
        // body ends the connection before the peer finishes.
        // The peer never terminates its chunked body; it only waits for the host to close.
        let (address, closed) = peer(move |socket| {
            request(socket);
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\nd\r\ndata: first\n\n\r\n")
                .unwrap();
        });
        let origin = format!("http://{address}");
        let result = call_then(
            &engine,
            install(std::slice::from_ref(&origin)),
            "first",
            json!(["http", address.to_string(), "/events"]),
            // Checked while the Store still exists, so the guest's body drop closed it.
            || {
                closed
                    .recv_timeout(Duration::from_secs(5))
                    .expect("dropping the body closed the connection");
            },
        );
        assert_eq!(result, json!({"tag": "ok", "val": "data: first\n\n"}));

        // An origin outside the grant is refused with the standard error code; nothing connects.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let other = listener.local_addr().unwrap();
        let result = call(
            &engine,
            install(&[origin]),
            "fetch",
            json!(["http", other.to_string(), "/"]),
        );
        assert_eq!(result["tag"], "err");
        assert!(
            result["val"]
                .as_str()
                .unwrap()
                .contains("HttpRequestDenied"),
            "{result}"
        );
        assert!(listener.accept().is_err());
    }
}

/// A raw socket grant to the same address never admits a `wasi:http` import.
#[test]
#[ignore = "requires TEMBRA_WASM_HTTP_CLIENT"]
fn raw_socket_grants_do_not_authorize_http() {
    let Some(component) = component() else {
        return;
    };
    let engine = Engine::new(Limits::default())
        .unwrap()
        .network("web")
        .unwrap();
    let mut install = Install::default();
    install.grants = Grants {
        network: vec![NetworkGrant {
            provider: "web".into(),
            connect: vec!["127.0.0.1:80".parse().unwrap()],
            ..NetworkGrant::default()
        }],
        ..Grants::default()
    };
    let error = engine.load(&artifact(&component), install).err().unwrap();
    assert!(error.message.contains("grant"), "{error}");
    assert_eq!(engine.instances(), 0);
}

/// Real HTTPS: DNS, a publicly trusted certificate and TLS for a compiled guest.
#[test]
#[ignore = "requires TEMBRA_WASM_HTTP_CLIENT and outbound HTTPS to example.com"]
fn compiled_components_reach_public_https() {
    if component().is_none() {
        return;
    }
    for engine in engines() {
        let result = call(
            &engine,
            install(&["https://example.com".into()]),
            "fetch",
            json!(["https", "example.com", "/"]),
        );
        let text = result["val"].as_str().unwrap();
        assert_eq!(result["tag"], "ok", "{result}");
        assert!(
            text.starts_with("200 ") && text.contains("Example Domain"),
            "{text}"
        );
    }
}

/// Start one fetcher `fetch` call under its own root; returns its context and completion.
fn start(
    pool: &mut LocalPool,
    host: &Host,
    reference: &Rc<tembra::Reference>,
    args: Value,
) -> (
    tembra::Context,
    futures::channel::oneshot::Receiver<tembra::Result<Value>>,
) {
    start_method(pool, host, reference, "fetch", args)
}

/// Start one fetcher call under its own root; returns its context and completion.
fn start_method(
    pool: &mut LocalPool,
    host: &Host,
    reference: &Rc<tembra::Reference>,
    method: &'static str,
    args: Value,
) -> (
    tembra::Context,
    futures::channel::oneshot::Receiver<tembra::Result<Value>>,
) {
    let (issued, context) = futures::channel::oneshot::channel();
    let (finished, finish) = futures::channel::oneshot::channel();
    let host = host.clone();
    let reference = reference.clone();
    futures::task::LocalSpawnExt::spawn_local(&pool.spawner(), async move {
        let result = host
            .root("alice", move |context| {
                async move {
                    let _ = issued.send(context.clone());
                    reference.call_value(&context, method, args).await
                }
                .boxed_local()
            })
            .await;
        let _ = finished.send(result);
    })
    .unwrap();
    (pool.run_until(context).unwrap(), finish)
}

/// Keep the host adapter serving (context checks) until `ready` holds.
fn serve_until(pool: &mut LocalPool, mut ready: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    pool.run_until(async {
        while !ready() {
            assert!(std::time::Instant::now() < deadline, "timed out");
            futures_timer::Delay::new(Duration::from_millis(2)).await;
        }
    });
}

/// A caller's cancellation drops its compiled call's in-flight send — before the request is
/// sent, and while the response head is awaited — and the call completes only after the
/// connection physically closed. The Store keeps serving later calls.
#[test]
#[ignore = "requires TEMBRA_WASM_HTTP_CLIENT"]
fn compiled_sends_are_cancelled_with_their_call() {
    if component().is_none() {
        return;
    }
    for engine in engines() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (headed, head_seen) = mpsc::channel();
        let (closed, observed) = mpsc::channel();
        thread::spawn(move || {
            for _ in 0..3 {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let mut head = Vec::new();
                let mut byte = [0];
                while !head.ends_with(b"\r\n\r\n") {
                    if socket.read(&mut byte).unwrap_or(0) == 0 {
                        break;
                    }
                    head.push(byte[0]);
                }
                let head = String::from_utf8(head).unwrap();
                if head.starts_with("GET /answer ") {
                    socket
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                        .unwrap();
                    let _ = socket.read_to_end(&mut Vec::new());
                    return;
                }
                let _ = headed.send(head);
                // Never answer: only the host closing the connection ends this read.
                let _ = socket.read_to_end(&mut Vec::new());
                let _ = closed.send(std::time::Instant::now());
            }
        });
        let mut pool = LocalPool::new();
        let host = Host::new(pool.spawner());
        let origin = format!("http://{address}");
        host.install(vec![
            engine
                .load(
                    &artifact(&component().unwrap()),
                    install(std::slice::from_ref(&origin)),
                )
                .unwrap(),
        ])
        .unwrap();
        let reference = Rc::new(host.get(SERVICE, vec!["main".into()], "alice").unwrap());
        pool.run_until(reference.ready()).unwrap();

        // Cancelled while the response head is awaited.
        let (context, finish) = start(
            &mut pool,
            &host,
            &reference,
            json!(["http", address.to_string(), "/stalled"]),
        );
        let mut head = None;
        serve_until(&mut pool, || {
            head = head_seen.try_recv().ok();
            head.is_some()
        });
        assert!(head.unwrap().starts_with("GET /stalled "));
        let cancelled = std::time::Instant::now();
        context.cancel().unwrap();
        let result = pool.run_until(finish).unwrap();
        let finished = std::time::Instant::now();
        let closed_at = observed.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(finished - cancelled < Duration::from_secs(2), "{result:?}");
        assert!(closed_at - cancelled < Duration::from_secs(2));
        // The guest saw its send fail (or the call surfaced the cancellation itself).
        match &result {
            Ok(value) => assert_eq!(value["tag"], "err", "{value}"),
            Err(error) => assert_eq!(error.code, tembra::Code::Cancelled, "{error}"),
        }

        // Cancelled before the guest ran: nothing is sent, or the connection closes at once.
        let (context, finish) = start(
            &mut pool,
            &host,
            &reference,
            json!(["http", address.to_string(), "/early"]),
        );
        context.cancel().unwrap();
        let _ = pool.run_until(finish).unwrap();

        // The same Store still serves a later call: it was not quarantined.
        let live = reference.clone();
        let args = json!(["http", address.to_string(), "/answer"]);
        let value = pool
            .run_until(host.root("alice", move |context| {
                async move { live.call_value(&context, "fetch", args).await }.boxed_local()
            }))
            .unwrap();
        assert_eq!(value, json!({"tag": "ok", "val": "200 ok"}));
        reference.release().unwrap();
        pool.run_until(host.close()).unwrap();
        assert_eq!(engine.instances(), 0);
    }
}

/// A response kept by one call stays open (revision-owned) while other calls run and complete,
/// and a later call that consumes and closes it closes the connection while it runs.
#[test]
#[ignore = "requires TEMBRA_WASM_HTTP_CLIENT"]
fn compiled_retained_response_is_closed_by_a_later_call() {
    if component().is_none() {
        return;
    }
    for engine in engines() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (closed, observed) = mpsc::channel();
        thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(20)))
                .unwrap();
            request(&mut socket);
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\nb\r\ndata: one\n\n\r\n")
                .unwrap();
            // Never end the stream: only the host closing the connection ends this read.
            let _ = socket.read_to_end(&mut Vec::new());
            let _ = closed.send(std::time::Instant::now());
        });
        let mut pool = LocalPool::new();
        let host = Host::new(pool.spawner());
        let origin = format!("http://{address}");
        host.install(vec![
            engine
                .load(
                    &artifact(&component().unwrap()),
                    install(std::slice::from_ref(&origin)),
                )
                .unwrap(),
        ])
        .unwrap();
        let reference = Rc::new(host.get(SERVICE, vec!["main".into()], "alice").unwrap());
        pool.run_until(reference.ready()).unwrap();
        let mut call = |method: &'static str, args: Value| {
            let live = reference.clone();
            pool.run_until(host.root("alice", move |context| {
                async move { live.call_value(&context, method, args).await }.boxed_local()
            }))
            .unwrap()
        };
        let kept = call("keep", json!(["http", address.to_string(), "/events"]));
        assert_eq!(kept, json!({"tag": "ok", "val": 200}));
        // Another call completes without waiting for the idle retained driver.
        let started = std::time::Instant::now();
        let other = call("fetch", json!(["http", "127.0.0.1:9", "/"]));
        assert_eq!(other["tag"], "err");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(
            observed.try_recv().is_err(),
            "the kept response is still open"
        );
        // A later call consumes and closes it; the connection closes while that call runs.
        let closed_value = call("close-kept", json!([]));
        let returned = std::time::Instant::now();
        assert_eq!(closed_value, json!({"tag": "ok", "val": "data: one\n\n"}));
        let closed_at = observed.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            closed_at < returned + Duration::from_millis(200),
            "closed {:?} after the closing call returned",
            closed_at.saturating_duration_since(returned)
        );
        reference.release().unwrap();
        pool.run_until(host.close()).unwrap();
        assert_eq!(engine.instances(), 0);
    }
}

/// A peer that answers `/answer` with "ok" and every other path with an SSE head plus one
/// event, then never ends that body; it reports each event write and each close by the host.
fn sse_peer() -> (
    SocketAddr,
    mpsc::Receiver<()>,
    mpsc::Receiver<std::time::Instant>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (evented, event_seen) = mpsc::channel();
    let (closed, observed) = mpsc::channel();
    thread::spawn(move || {
        while let Ok((mut socket, _)) = listener.accept() {
            socket
                .set_read_timeout(Some(Duration::from_secs(30)))
                .unwrap();
            let head = request(&mut socket);
            if head.starts_with("GET /answer ") {
                let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
                let _ = socket.read_to_end(&mut Vec::new());
                continue;
            }
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\nb\r\ndata: one\n\n\r\n")
                .unwrap();
            let _ = evented.send(());
            let _ = socket.read_to_end(&mut Vec::new());
            let _ = closed.send(std::time::Instant::now());
        }
    });
    (address, event_seen, observed)
}

/// Install the fixture, granted for `address`, and return a ready reference.
fn installed(
    engine: &Engine,
    pool: &mut LocalPool,
    address: SocketAddr,
) -> (Host, Rc<tembra::Reference>) {
    let host = Host::new(pool.spawner());
    host.install(vec![
        engine
            .load(
                &artifact(&component().unwrap()),
                install(&[format!("http://{address}")]),
            )
            .unwrap(),
    ])
    .unwrap();
    let reference = Rc::new(host.get(SERVICE, vec!["main".into()], "alice").unwrap());
    pool.run_until(reference.ready()).unwrap();
    (host, reference)
}

/// Call one method under its own root and wait for the result.
fn invoke_on(
    pool: &mut LocalPool,
    host: &Host,
    reference: &Rc<tembra::Reference>,
    method: &'static str,
    args: Value,
) -> Value {
    let live = reference.clone();
    pool.run_until(host.root("alice", move |context| {
        async move { live.call_value(&context, method, args).await }.boxed_local()
    }))
    .unwrap()
}

/// Cancelling a compiled call while its guest is parked in a body read (SSE head plus one
/// event, then idle) closes the body at once: the call finishes promptly, the peer sees the
/// close, and the same Store serves a later call. Covered with the response-handling future
/// already resolved (`fetch`) and still pending (`stream-held`).
#[test]
#[ignore = "requires TEMBRA_WASM_HTTP_CLIENT"]
fn compiled_active_body_reads_end_with_their_cancelled_call() {
    if component().is_none() {
        return;
    }
    for engine in engines() {
        for method in ["fetch", "stream-held"] {
            let (address, event_seen, observed) = sse_peer();
            let mut pool = LocalPool::new();
            let (host, reference) = installed(&engine, &mut pool, address);
            let (context, finish) = start_method(
                &mut pool,
                &host,
                &reference,
                method,
                json!(["http", address.to_string(), "/events"]),
            );
            serve_until(&mut pool, || event_seen.try_recv().is_ok());
            // Let the guest take the head and the event and park in its next body read.
            let settle = std::time::Instant::now() + Duration::from_millis(300);
            serve_until(&mut pool, || std::time::Instant::now() >= settle);
            let cancelled = std::time::Instant::now();
            context.cancel().unwrap();
            let result = pool.run_until(finish).unwrap();
            let finished = cancelled.elapsed();
            let closed = observed
                .recv_timeout(Duration::from_secs(5))
                .expect("the host closed the connection")
                .saturating_duration_since(cancelled);
            // A watchdog against waiting for the call deadline, not a performance bound.
            assert!(
                finished < Duration::from_secs(2),
                "{method}: {finished:?} {result:?}"
            );
            assert!(closed < Duration::from_secs(2), "{method}: {closed:?}");
            if let Err(error) = &result {
                assert_eq!(error.code, tembra::Code::Cancelled, "{method}: {error}");
            }
            let later = invoke_on(
                &mut pool,
                &host,
                &reference,
                "fetch",
                json!(["http", address.to_string(), "/answer"]),
            );
            assert_eq!(later, json!({"tag": "ok", "val": "200 ok"}), "{method}");
            reference.release().unwrap();
            pool.run_until(host.close()).unwrap();
            assert_eq!(engine.instances(), 0);
        }
    }
}

/// A body the call read and left open is closed when that call returns normally, and the call
/// returns only after the close; an `err` written to the response-handling future closes a body
/// the guest still holds (no body drop involved).
#[test]
#[ignore = "requires TEMBRA_WASM_HTTP_CLIENT"]
fn compiled_active_bodies_close_with_their_call_or_handling_error() {
    if component().is_none() {
        return;
    }
    for engine in engines() {
        let (address, _event_seen, observed) = sse_peer();
        let mut pool = LocalPool::new();
        let (host, reference) = installed(&engine, &mut pool, address);
        let args = json!(["http", address.to_string(), "/events"]);
        let value = invoke_on(&mut pool, &host, &reference, "read-and-keep", args.clone());
        assert_eq!(value, json!({"tag": "ok", "val": "data: one\n\n"}));
        observed
            .recv_timeout(Duration::from_secs(2))
            .expect("the kept active body closed with its call");
        let value = invoke_on(&mut pool, &host, &reference, "hold-close", args);
        assert_eq!(value, json!({"tag": "ok", "val": "data: one\n\n|0"}));
        observed
            .recv_timeout(Duration::from_secs(2))
            .expect("the handling error closed the held body");
        let later = invoke_on(
            &mut pool,
            &host,
            &reference,
            "fetch",
            json!(["http", address.to_string(), "/answer"]),
        );
        assert_eq!(later, json!({"tag": "ok", "val": "200 ok"}));
        reference.release().unwrap();
        pool.run_until(host.close()).unwrap();
        assert_eq!(engine.instances(), 0);
    }
}

/// The cancellation window between a response head and the first body read, made deterministic:
/// the guest takes the `/window` head, then waits on a `/barrier` request that the peer holds
/// until the caller has cancelled, and only then reads the body. A body whose first read follows
/// the cancellation must be closed when it registers; the cooperative reader then returns, the
/// peer sees the close, and the Store keeps serving (no 10 s deadline, no quarantine).
#[test]
#[ignore = "requires TEMBRA_WASM_HTTP_CLIENT"]
fn compiled_body_first_read_after_cancellation_is_closed() {
    if component().is_none() {
        return;
    }
    for engine in engines() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (barrier_seen, barrier) = mpsc::channel();
        let (window_closed, closed) = mpsc::channel();
        thread::spawn(move || {
            while let Ok((mut socket, _)) = listener.accept() {
                let (barrier_seen, window_closed) = (barrier_seen.clone(), window_closed.clone());
                thread::spawn(move || {
                    socket
                        .set_read_timeout(Some(Duration::from_secs(30)))
                        .unwrap();
                    let head = request(&mut socket);
                    if head.starts_with("GET /answer ") {
                        let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
                    } else if head.starts_with("GET /barrier ") {
                        // Held until the host drops this request (the cancelled send).
                        let _ = barrier_seen.send(());
                    } else {
                        let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\nb\r\ndata: one\n\n\r\n");
                    }
                    let _ = socket.read_to_end(&mut Vec::new());
                    if head.starts_with("GET /window ") {
                        let _ = window_closed.send(std::time::Instant::now());
                    }
                });
            }
        });
        let mut pool = LocalPool::new();
        let (host, reference) = installed(&engine, &mut pool, address);
        let (context, finish) = start_method(
            &mut pool,
            &host,
            &reference,
            "read-after-barrier",
            json!(["http", address.to_string(), "/window", "/barrier"]),
        );
        // The `/window` head was delivered and the guest now waits on the barrier, so it has not
        // read the body yet.
        serve_until(&mut pool, || barrier.try_recv().is_ok());
        let cancelled = std::time::Instant::now();
        context.cancel().unwrap();
        let result = pool.run_until(finish).unwrap();
        let finished = cancelled.elapsed();
        let closed_after = closed
            .recv_timeout(Duration::from_secs(5))
            .expect("the window body's connection closed")
            .saturating_duration_since(cancelled);
        // A watchdog against waiting for the call deadline, not a performance bound.
        assert!(finished < Duration::from_secs(2), "{finished:?} {result:?}");
        assert!(closed_after < Duration::from_secs(2), "{closed_after:?}");
        if let Err(error) = &result {
            assert_eq!(error.code, tembra::Code::Cancelled, "{error}");
        }
        let later = invoke_on(
            &mut pool,
            &host,
            &reference,
            "fetch",
            json!(["http", address.to_string(), "/answer"]),
        );
        assert_eq!(later, json!({"tag": "ok", "val": "200 ok"}));
        reference.release().unwrap();
        pool.run_until(host.close()).unwrap();
        assert_eq!(engine.instances(), 0);
    }
}
