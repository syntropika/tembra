//! Source Orbs use standard `wasi:sockets` TCP through the native host, end to end with the real
//! interpreter: exact connect grants, incremental receive, and the standard resource lifetimes.
//!
//! Lifetime rules exercised here:
//! - a `tcp-socket`, its `receive` stream and the stream's completion future are separate owned
//!   resources; dropping the socket alone keeps the stream readable;
//! - a connection closes once the guest drops every resource that keeps it open;
//! - a cancelled callback's pending read settles promptly, its `finally` cleanup still runs, and
//!   the drops it makes close the connection before the callback retires;
//! - none of this waits for an I/O deadline, and the next callback is unaffected.
//!
//! The raw guest calls the generated standard bindings (`__tembraStandard`). The authored
//! TypeScript path is covered by `packages/ts/sdk/test/sockets.test.ts`.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::{
    io::{ErrorKind, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    rc::Rc,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use tembra::{Host, Reference};
use tembra_executor_wasm::{Contract, Engine, Grants, Install, Limits, NetworkGrant};

const SERVICE: &str = "raw::test:sockets/net";
const WIT: &str = "package test:sockets; interface net { fetch: async func(port: u16) -> string; alive: async func(port: u16) -> string; abandon: async func(port: u16) -> bool; hold: async func(port: u16) -> bool; forbidden: async func(port: u16) -> bool; } world app { import wasi:sockets/types@0.3.0; export net; }";
const SOURCE: &str = r#"const sockets = __tembraStandard['wasi:sockets/types@0.3.0'];
const dispose = Symbol.dispose ?? Symbol.for('Symbol.dispose');
// Chunked access shared with the SDK: one host read per call, one host write per chunk.
const READ_CHUNK = Symbol.for('tembra.wasi.stream.read-chunk');
const WRITE_CHUNKS = Symbol.for('tembra.wasi.stream.write-chunks');
const ascii = (text) => Uint8Array.from(text, (character) => character.charCodeAt(0));
const text = (bytes) => String.fromCharCode(...bytes);
const connect = async (port) => {
    const socket = sockets.TcpSocket.create('ipv4');
    try {
        await socket.connect({ tag: 'ipv4', val: { port, address: [127, 0, 0, 1] } });
    } catch (error) {
        socket[dispose]();
        throw error;
    }
    return socket;
};
const drain = async (stream) => {
    let all = '';
    for (let chunk = await stream[READ_CHUNK](); chunk; chunk = await stream[READ_CHUNK]())
        all += text(chunk);
    return all;
};
globalThis.__tembraOrb = { services: { net: { methods: {
    // Send PING, receive the first chunk, and only then send ACK: the peer sends the rest of
    // its answer after ACK, so the first chunk was delivered before the rest existed.
    async fetch(self, context, port) {
        const socket = await connect(port);
        const [stream, done] = socket.receive();
        let seen;
        const first = new Promise((resolve) => { seen = resolve; });
        const sent = socket.send({ async *[WRITE_CHUNKS]() {
            yield ascii('PING\n');
            await first;
            yield ascii('ACK\n');
        } });
        const head = await stream[READ_CHUNK]();
        seen();
        const rest = await drain(stream);
        await sent;
        await done;
        stream[dispose]();
        done[dispose]();
        socket[dispose]();
        return JSON.stringify({ first: head ? text(head) : null, all: (head ? text(head) : '') + rest });
    },
    // Dropping the socket alone releases only its own reference: the stream still reads.
    async alive(self, context, port) {
        const socket = await connect(port);
        const [stream, done] = socket.receive();
        socket[dispose]();
        const all = await drain(stream);
        await done;
        stream[dispose]();
        done[dispose]();
        return all;
    },
    // A read is pending when the guest drops the stream, its future and the socket: dropping
    // the last references closes the connection although the peer never sent anything.
    async abandon(self, context, port) {
        const socket = await connect(port);
        const [stream, done] = socket.receive();
        stream[READ_CHUNK]().catch(() => {});
        stream[dispose]();
        done[dispose]();
        socket[dispose]();
        return true;
    },
    // Waits for data that never comes; the caller cancels. The pending read is rejected and
    // the guest's cleanup drops every resource of the connection.
    async hold(self, context, port) {
        const socket = await connect(port);
        const [stream, done] = socket.receive();
        try {
            await stream[READ_CHUNK]();
            return true;
        } finally {
            stream[dispose]();
            done[dispose]();
            socket[dispose]();
        }
    },
    async forbidden(self, context, port) {
        try { (await connect(port))[dispose](); return false; } catch { return true; }
    }
} } } };"#;

type Script = fn(&mut TcpStream);

/// Serve scripted peers in order and report when each observes the host's close.
fn peers(scripts: Vec<Script>) -> (SocketAddr, mpsc::Receiver<Instant>) {
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

/// Answer PING with PO, wait for the guest's ACK (sent only after it received data), then send
/// NG and end the stream.
fn answer(socket: &mut TcpStream) {
    let mut request = [0; 5];
    socket.read_exact(&mut request).unwrap();
    assert_eq!(&request, b"PING\n");
    socket.write_all(b"PO").unwrap();
    let mut ack = [0; 4];
    socket.read_exact(&mut ack).unwrap();
    assert_eq!(&ack, b"ACK\n");
    socket.write_all(b"NG").unwrap();
    socket.shutdown(std::net::Shutdown::Write).unwrap();
}

/// Send after the guest has had time to drop its socket handle, then end the stream.
fn later(socket: &mut TcpStream) {
    thread::sleep(Duration::from_millis(100));
    socket.write_all(b"ALIVE").unwrap();
    socket.shutdown(std::net::Shutdown::Write).unwrap();
}

/// Never answer.
fn silent(_: &mut TcpStream) {}

#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE"]
fn source_sockets_stream_incrementally_and_follow_standard_lifetimes() {
    source_sockets(false);
}

#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE"]
fn interpreted_source_sockets_stream_incrementally_and_follow_standard_lifetimes() {
    source_sockets(true);
}

fn source_sockets(interpreted: bool) {
    let interpreter =
        std::fs::read(std::env::var_os("TEMBRA_WASM_ENGINE").expect("TEMBRA_WASM_ENGINE")).unwrap();
    let (address, closed) = peers(vec![answer, later, silent, silent, answer]);
    let ungranted = TcpListener::bind("127.0.0.1:0").unwrap();
    ungranted.set_nonblocking(true).unwrap();
    let limits = Limits {
        // Debug Pulley is slower; every physical close below keeps its own two-second gate.
        timeout_ms: if interpreted { 30_000 } else { 10_000 },
        ..Limits::default()
    };
    let engine = if interpreted {
        Engine::interpreted(limits)
    } else {
        Engine::new(limits)
    }
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
    let contract = Contract::from_source("sockets.wit", WIT, Some("app")).unwrap();
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
    let fetch = |pool: &mut LocalPool| {
        let reply = call(pool, &reference, "fetch", address.port()).unwrap();
        let reply: Value = serde_json::from_str(reply.as_str().unwrap()).unwrap();
        // TCP keeps no chunk boundaries: the complete answer, and incremental delivery (the
        // first chunk arrived before the peer had written the rest), are checked separately.
        assert_eq!(reply["all"], "PONG", "{reply}");
        let first = reply["first"].as_str().unwrap();
        assert!(
            !first.is_empty() && "PO".starts_with(first),
            "first chunk {first:?} precedes the rest"
        );
    };

    fetch(&mut pool);
    closed.recv_timeout(Duration::from_secs(5)).unwrap();

    // Exact grants: an ungranted address is refused with no connection attempt reaching it.
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
    assert_eq!(
        ungranted.accept().map(|_| ()).unwrap_err().kind(),
        ErrorKind::WouldBlock,
        "the ungranted peer saw no connection"
    );

    // Dropping the socket alone keeps its receive stream readable to the end.
    assert_eq!(
        call(&mut pool, &reference, "alive", address.port()).unwrap(),
        "ALIVE"
    );
    closed.recv_timeout(Duration::from_secs(5)).unwrap();

    // Dropping every resource, with a read still pending, closes the connection by the time
    // the callback completes, not at an I/O deadline.
    let started = Instant::now();
    assert_eq!(
        call(&mut pool, &reference, "abandon", address.port()).unwrap(),
        true
    );
    let peer_closed = closed.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(peer_closed.duration_since(started) < Duration::from_secs(2));

    // Cancelling the caller settles the pending read promptly; the guest's cleanup drops close
    // the connection physically.
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
    assert!(outcome.is_err(), "{outcome:?}");
    let settled = started.elapsed();
    let peer_closed = closed.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        peer_closed.duration_since(started) < Duration::from_secs(2),
        "the cancelled callback's connection closed promptly"
    );
    assert!(
        settled < Duration::from_secs(2),
        "cancel settled in {settled:?}"
    );

    // The next callback is unaffected (no quarantine, no leftover state).
    fetch(&mut pool);
    closed.recv_timeout(Duration::from_secs(5)).unwrap();

    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "work waited for an I/O deadline"
    );
    assert_eq!(engine.instances(), 0);
}
