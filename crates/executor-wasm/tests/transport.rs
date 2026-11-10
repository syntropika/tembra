//! Source Orbs read and write raw byte streams incrementally through the native host.
//!
//! The guest owns any protocol; the host only connects exact granted addresses and moves bytes.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener},
    rc::Rc,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use tembra::{Host, Reference};
use tembra_executor_wasm::{Contract, Engine, Grants, Install, Limits, NetworkGrant};

const SERVICE: &str = "raw::test:transport/net";
const WIT: &str = "package test:transport; interface net { fetch: async func(port: u16) -> string; abandon: async func(port: u16) -> bool; hold: async func(port: u16) -> bool; forbidden: async func(port: u16) -> bool; } world app { import wasi:sockets/types@0.3.0; export net; }";
const SOURCE: &str = r#"const ascii = (text) => Uint8Array.from(text, (character) => character.charCodeAt(0));
globalThis.__tembraOrb = { services: { net: { methods: {
    async fetch(self, context, port) {
        const stream = await tembra.transport.connect('127.0.0.1', port);
        await stream.write(ascii('PING\n'));
        const chunks = [];
        for (let chunk = await stream.read(); chunk !== null; chunk = await stream.read())
            chunks.push(String.fromCharCode(...chunk));
        await stream.close();
        return chunks.join('|');
    },
    async abandon(self, context, port) {
        const stream = await tembra.transport.connect('127.0.0.1', port);
        stream.read().catch(() => {});
        return true;
    },
    async hold(self, context, port) {
        const stream = await tembra.transport.connect('127.0.0.1', port);
        await stream.read();
        return true;
    },
    async forbidden(self, context, port) {
        try { await tembra.transport.connect('127.0.0.1', port); return false; } catch { return true; }
    }
} } } };"#;

/// Serve scripted peers in order and report when each observes the host's close.
fn peers(scripts: Vec<fn(&mut std::net::TcpStream)>) -> (SocketAddr, mpsc::Receiver<Instant>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (closed, observed) = mpsc::channel();
    thread::spawn(move || {
        for script in scripts {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            script(&mut socket);
            let mut rest = Vec::new();
            let _ = socket.read_to_end(&mut rest);
            let _ = closed.send(Instant::now());
        }
    });
    (address, observed)
}

/// Answer a line in two separate writes, then end the stream.
fn answer(socket: &mut std::net::TcpStream) {
    let mut request = [0; 5];
    socket.read_exact(&mut request).unwrap();
    assert_eq!(&request, b"PING\n");
    socket.write_all(b"PO").unwrap();
    thread::sleep(Duration::from_millis(50));
    socket.write_all(b"NG").unwrap();
    socket.shutdown(std::net::Shutdown::Write).unwrap();
}

/// Never answer.
fn silent(_: &mut std::net::TcpStream) {}

#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE"]
fn source_streams_are_incremental_and_close_with_their_callback() {
    let interpreter =
        std::fs::read(std::env::var_os("TEMBRA_WASM_ENGINE").expect("TEMBRA_WASM_ENGINE")).unwrap();
    let (address, closed) = peers(vec![answer, silent, silent]);
    let ungranted = TcpListener::bind("127.0.0.1:0").unwrap();
    let engine = Engine::new(Limits::default())
        .unwrap()
        .interpreter(&interpreter)
        .unwrap()
        .network("tcp")
        .unwrap();
    let mut install = Install::default();
    install.grants = Grants {
        network: vec![NetworkGrant {
            provider: "tcp".into(),
            connect: vec![address],
            ..Default::default()
        }],
        ..Grants::default()
    };
    let contract = Contract::from_source("transport.wit", WIT, Some("app")).unwrap();
    let orb = engine
        .load(
            &contract.source_artifact(SOURCE, "raw", 1).unwrap(),
            install,
        )
        .unwrap();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![orb]).unwrap();
    let reference = Rc::new(host.get(SERVICE, vec!["main".into()], "alice").unwrap());
    pool.run_until(reference.ready()).unwrap();
    let call = |pool: &mut LocalPool, reference: &Rc<Reference>, method: &'static str, port| {
        let reference = reference.clone();
        pool.run_until(host.root("alice", move |context| {
            async move {
                reference
                    .call::<_, Value>(&context, method, json!([port]))
                    .await
            }
            .boxed_local()
        }))
    };
    // Bytes written in two pieces arrive as separate reads, then end of stream.
    assert_eq!(
        call(&mut pool, &reference, "fetch", address.port()).unwrap(),
        "PO|NG"
    );
    closed.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(
        call(
            &mut pool,
            &reference,
            "forbidden",
            ungranted.local_addr().unwrap().port()
        )
        .unwrap(),
        true
    );
    // A read the guest abandoned settles when its callback completes, not at its deadline.
    let started = Instant::now();
    assert_eq!(
        call(&mut pool, &reference, "abandon", address.port()).unwrap(),
        true
    );
    let peer_closed = closed.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(peer_closed.duration_since(started) < Duration::from_secs(2));
    // Cancelling the caller closes the callback's stream, so its pending read settles promptly.
    let held = reference.clone();
    let started = Instant::now();
    let outcome = pool.run_until(host.root("alice", move |context| {
        async move {
            let cancel = context.clone();
            let (result, ()) = futures::join!(
                held.call::<_, Value>(&context, "hold", json!([address.port()])),
                async move {
                    futures_timer::Delay::new(Duration::from_millis(100)).await;
                    cancel.cancel().unwrap();
                }
            );
            result
        }
        .boxed_local()
    }));
    assert!(outcome.is_err());
    let peer_closed = closed.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(peer_closed.duration_since(started) < Duration::from_secs(2));
    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "work waited for an I/O deadline"
    );
    assert_eq!(engine.instances(), 0);
}
