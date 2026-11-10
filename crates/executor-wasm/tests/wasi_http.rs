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
