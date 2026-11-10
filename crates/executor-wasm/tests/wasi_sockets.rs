//! A compiled component uses standard `wasi:sockets` through the host's upstream implementation:
//! TCP idle drop, UDP, listen/accept, Tembra's name-lookup override, and pending socket waits
//! released by the caller's cancellation. Every scenario runs on Cranelift and Pulley.
//!
//! The guest (`rust-sockets`, built with `just build-sockets`) uses official wit-bindgen
//! resources; peers are real loopback sockets.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream, UdpSocket},
    rc::Rc,
    thread,
    time::{Duration, Instant},
};
use tembra::Host;
use tembra_executor_wasm::{Contract, Engine, Grants, Install, Limits, NetworkGrant};

const SERVICE: &str = "net::example:sockets/probe@1.0.0";

fn component() -> Option<Vec<u8>> {
    let path = std::env::var_os("TEMBRA_WASM_SOCKETS").filter(|path| !path.is_empty());
    let Some(path) = path else {
        eprintln!("skipped: TEMBRA_WASM_SOCKETS names no compiled sockets fixture");
        return None;
    };
    Some(std::fs::read(path).unwrap())
}

fn artifact(component: &[u8]) -> Vec<u8> {
    Contract::from_dir(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/wit/sockets"),
        Some("sockets"),
    )
    .unwrap()
    .component_artifact(component, "net", 1, &Default::default())
    .unwrap()
}

fn engines() -> [(&'static str, Engine); 2] {
    [
        (
            "cranelift",
            Engine::new(Limits::default())
                .unwrap()
                .network("net")
                .unwrap(),
        ),
        (
            "pulley",
            Engine::interpreted(Limits::default())
                .unwrap()
                .network("net")
                .unwrap(),
        ),
    ]
}

/// One installed Orb; its Store lives until `close`.
struct Orb {
    pool: LocalPool,
    host: Host,
    reference: Rc<tembra::Reference>,
}

impl Orb {
    fn install(engine: &Engine, artifact: &[u8], grant: NetworkGrant) -> Self {
        let mut install = Install::default();
        install.grants = Grants {
            network: vec![NetworkGrant {
                provider: "net".into(),
                ..grant
            }],
            ..Grants::default()
        };
        let mut pool = LocalPool::new();
        let host = Host::new(pool.spawner());
        host.install(vec![engine.load(artifact, install).unwrap()])
            .unwrap();
        let reference = Rc::new(host.get(SERVICE, vec!["main".into()], "alice").unwrap());
        pool.run_until(reference.ready()).unwrap();
        Self {
            pool,
            host,
            reference,
        }
    }

    fn call(&mut self, method: &'static str, args: Value) -> tembra::Result<Value> {
        let reference = self.reference.clone();
        self.pool.run_until(self.host.root("alice", move |context| {
            async move {
                reference
                    .call_portable::<_, serde_json::Value>(&context, method, args)
                    .await
            }
            .boxed_local()
        }))
    }

    /// Start a call under its own root; returns its context and completion.
    fn start(
        &mut self,
        method: &'static str,
        args: Value,
    ) -> (
        tembra::Context,
        futures::channel::oneshot::Receiver<tembra::Result<Value>>,
    ) {
        let (issued, context) = futures::channel::oneshot::channel();
        let (finished, finish) = futures::channel::oneshot::channel();
        let host = self.host.clone();
        let reference = self.reference.clone();
        futures::task::LocalSpawnExt::spawn_local(&self.pool.spawner(), async move {
            let result = host
                .root("alice", move |context| {
                    async move {
                        let _ = issued.send(context.clone());
                        reference
                            .call_portable::<_, serde_json::Value>(&context, method, args)
                            .await
                    }
                    .boxed_local()
                })
                .await;
            let _ = finished.send(result);
        })
        .unwrap();
        (self.pool.run_until(context).unwrap(), finish)
    }

    fn close(self, engine: &Engine) {
        self.reference.release().unwrap();
        let mut pool = self.pool;
        pool.run_until(self.host.close()).unwrap();
        assert_eq!(engine.instances(), 0);
    }
}

fn ok(value: tembra::Result<Value>) -> Value {
    let value = value.expect("call succeeds");
    assert_eq!(value["tag"], "ok", "{value}");
    value["val"].clone()
}

fn err(value: tembra::Result<Value>) -> String {
    let value = value.expect("call succeeds");
    assert_eq!(value["tag"], "err", "{value}");
    value["val"].as_str().unwrap().to_owned()
}

/// A loopback port nobody listens on right now.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// TCP idle drop, UDP with a granted and a denied peer, listen/accept of real clients, and name
/// lookup through Tembra's override (its denial is `access-denied`, which upstream's own lookup
/// never answers).
#[test]
#[ignore = "requires TEMBRA_WASM_SOCKETS"]
fn compiled_components_use_standard_wasi_sockets() {
    let Some(component) = component() else {
        return;
    };
    let artifact = artifact(&component);
    for (name, engine) in engines() {
        // Idle drop: the peer sees end of stream while the Orb stays installed.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let peer = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            socket.write_all(b"hello").unwrap();
            matches!(socket.read(&mut [0; 1]), Ok(0))
        });
        let mut orb = Orb::install(
            &engine,
            &artifact,
            NetworkGrant {
                connect: vec![address],
                ..Default::default()
            },
        );
        assert_eq!(ok(orb.call("read-drop", json!([address.port()]))), "hello");
        assert!(peer.join().unwrap(), "{name}: idle peer saw end of stream");
        orb.close(&engine);

        // UDP: `create` is WIT-synchronous and Rust-async upstream.
        let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
        let address = peer.local_addr().unwrap();
        let mut orb = Orb::install(
            &engine,
            &artifact,
            NetworkGrant {
                datagrams: vec![address],
                ..Default::default()
            },
        );
        // The peer's receive bound starts once the Orb is ready: installation may recompile the
        // component after the previous Orb released its code, and that is not peer wait time.
        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let echo = thread::spawn(move || {
            let mut buffer = [0; 64];
            let (count, from) = peer.recv_from(&mut buffer).unwrap();
            peer.send_to(&[b"echo:", &buffer[..count]].concat(), from)
                .unwrap();
        });
        assert_eq!(
            ok(orb.call("udp-echo", json!([address.port(), "ping"]))),
            "echo:ping"
        );
        echo.join().unwrap();
        let denied = err(orb.call("udp-echo", json!([free_port(), "no"])));
        assert!(denied.contains("AccessDenied"), "{name}: {denied}");
        orb.close(&engine);

        // Listen and accept three clients; the listener closes with the guest's drops.
        let port = free_port();
        let mut orb = Orb::install(
            &engine,
            &artifact,
            NetworkGrant {
                listen: vec![SocketAddr::from(([127, 0, 0, 1], port))],
                ..Default::default()
            },
        );
        let (_, finish) = orb.start("serve", json!([port, 3]));
        let clients: Vec<_> = (0..3)
            .map(|index| {
                thread::spawn(move || {
                    let deadline = Instant::now() + Duration::from_secs(5);
                    let mut client = loop {
                        match TcpStream::connect(("127.0.0.1", port)) {
                            Ok(client) => break client,
                            Err(_) if Instant::now() < deadline => {
                                thread::sleep(Duration::from_millis(5))
                            }
                            Err(error) => panic!("client {index}: {error}"),
                        }
                    };
                    client
                        .write_all(format!("client-{index}").as_bytes())
                        .unwrap();
                    client.shutdown(std::net::Shutdown::Write).unwrap();
                    client
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    matches!(client.read(&mut [0; 1]), Ok(0))
                })
            })
            .collect();
        let texts = ok(orb.pool.run_until(finish).unwrap());
        let mut texts: Vec<String> = serde_json::from_value(texts).unwrap();
        texts.sort();
        assert_eq!(texts, ["client-0", "client-1", "client-2"], "{name}");
        for client in clients {
            assert!(
                client.join().unwrap(),
                "{name}: client saw the guest's close"
            );
        }
        assert!(
            TcpStream::connect_timeout(
                &SocketAddr::from(([127, 0, 0, 1], port)),
                Duration::from_secs(1)
            )
            .is_err(),
            "{name}: the listener closed while the Orb stays installed"
        );
        assert_eq!(engine.instances(), 1);
        orb.close(&engine);

        // Name lookup through the Tembra override.
        let mut orb = Orb::install(
            &engine,
            &artifact,
            NetworkGrant {
                resolve: true,
                ..Default::default()
            },
        );
        assert_eq!(
            ok(orb.call("resolve", json!(["127.0.0.1"]))),
            json!(["127.0.0.1"])
        );
        let localhost = ok(orb.call("resolve", json!(["localhost"])));
        assert!(
            localhost.as_array().is_some_and(|all| !all.is_empty()),
            "{name}: {localhost}"
        );
        assert!(err(orb.call("resolve", json!(["a b"]))).contains("InvalidArgument"));
        orb.close(&engine);
        let mut orb = Orb::install(
            &engine,
            &artifact,
            NetworkGrant {
                connect: vec!["127.0.0.1:9".parse().unwrap()],
                ..Default::default()
            },
        );
        let denied = err(orb.call("resolve", json!(["localhost"])));
        assert!(denied.contains("AccessDenied"), "{name}: {denied}");
        orb.close(&engine);
    }
}

/// A loopback listener whose accept queue is full, so a further connect stays pending (its SYN
/// is dropped), plus the connections that fill it.
fn saturated() -> (TcpListener, Vec<TcpStream>, SocketAddr) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .unwrap();
    let listener = {
        let _entered = runtime.enter();
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        socket.listen(0).unwrap().into_std().unwrap()
    };
    let address = listener.local_addr().unwrap();
    let mut fillers = Vec::new();
    while let Ok(filler) = TcpStream::connect_timeout(&address, Duration::from_millis(300)) {
        fillers.push(filler);
        assert!(fillers.len() < 64, "accept queue never filled");
    }
    (listener, fillers, address)
}

/// Cancel a call while its guest waits on a pending socket operation: the call settles at once
/// with the standard `other("call cancelled")` error the guest saw, and the Store serves the
/// next call (it was not quarantined).
fn cancel_pending(orb: &mut Orb, method: &'static str, args: Value) -> String {
    let message = err(cancel_after(orb, method, args).0);
    assert!(message.contains("call cancelled"), "{message}");
    assert_eq!(
        ok(orb.call("resolve", json!(["127.0.0.1"]))),
        json!(["127.0.0.1"]),
        "the Store keeps serving"
    );
    message
}

/// Start a call, let its guest reach its pending operation, cancel it, and return the call's
/// result and how long it took to settle after the cancellation.
fn cancel_after(
    orb: &mut Orb,
    method: &'static str,
    args: Value,
) -> (tembra::Result<Value>, Duration) {
    let (context, finish) = orb.start(method, args);
    orb.pool
        .run_until(futures_timer::Delay::new(Duration::from_millis(300)));
    let cancelled = Instant::now();
    context.cancel().unwrap();
    let result = orb.pool.run_until(finish).unwrap();
    let settled = cancelled.elapsed();
    assert!(
        settled < Duration::from_secs(2),
        "{method}: settled {settled:?}"
    );
    (result, settled)
}

/// A caller's cancellation releases a compiled call waiting on a pending TCP connect or UDP
/// receive. The dropped connect leaves no physical work: once the peer's queue drains, no late
/// connection arrives, because the guest dropped its socket after the cancelled connect.
#[test]
#[ignore = "requires TEMBRA_WASM_SOCKETS"]
fn compiled_socket_waits_end_with_their_cancelled_call() {
    let Some(component) = component() else {
        return;
    };
    let artifact = artifact(&component);
    for (name, engine) in engines() {
        let (listener, fillers, address) = saturated();
        let silent = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut orb = Orb::install(
            &engine,
            &artifact,
            NetworkGrant {
                connect: vec![address],
                datagrams: vec![silent.local_addr().unwrap()],
                resolve: true,
                ..Default::default()
            },
        );
        let connect = cancel_pending(&mut orb, "connect", json!([address.port()]));
        assert!(connect.starts_with("connect:"), "{name}: {connect}");
        let receive = cancel_pending(
            &mut orb,
            "udp-echo",
            json!([silent.local_addr().unwrap().port(), "silence"]),
        );
        assert!(receive.starts_with("receive:"), "{name}: {receive}");
        // A socket still retrying its SYN would connect once the queue drains.
        listener.set_nonblocking(true).unwrap();
        let (mut drained, mut late) = (0, 0);
        let deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < deadline {
            match listener.accept() {
                Ok(_) if drained < fillers.len() => drained += 1,
                Ok(_) => late += 1,
                Err(_) => thread::sleep(Duration::from_millis(20)),
            }
        }
        assert_eq!(
            late, 0,
            "{name}: the cancelled connect left no connection attempt"
        );
        orb.close(&engine);
        drop(fillers);
    }
}

/// A peer that accepts one connection and reports whether the host closed it (end of stream or
/// a reset) within the bound, without ever writing.
fn idle_peer() -> (SocketAddr, thread::JoinHandle<bool>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let peer = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        closed(&mut socket)
    });
    (address, peer)
}

/// Whether the host closed `socket` (end of stream or reset), not merely went quiet.
fn closed(socket: &mut TcpStream) -> bool {
    match socket.read(&mut [0; 1]) {
        Ok(0) => true,
        Ok(_) => false,
        Err(error) => !matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ),
    }
}

/// A caller's cancellation ends a compiled call whose guest waits in a TCP stream read or an
/// accept read: the read returns at once (end of stream), the call settles promptly with the
/// stream's direction physically closed, and the Store serves the next call.
#[test]
#[ignore = "requires TEMBRA_WASM_SOCKETS"]
fn compiled_socket_stream_reads_end_with_their_cancelled_call() {
    let Some(component) = component() else {
        return;
    };
    let artifact = artifact(&component);
    for (name, engine) in engines() {
        // Pending read: the peer never writes; it sees the connection close.
        let (address, peer) = idle_peer();
        let port = free_port();
        let mut orb = Orb::install(
            &engine,
            &artifact,
            NetworkGrant {
                connect: vec![address],
                listen: vec![SocketAddr::from(([127, 0, 0, 1], port))],
                resolve: true,
                ..Default::default()
            },
        );
        let (result, _) = cancel_after(&mut orb, "read-pending", json!([address.port()]));
        assert_eq!(ok(result), "end", "{name}: the pending read ended");
        assert!(peer.join().unwrap(), "{name}: the read direction closed");
        assert_eq!(
            ok(orb.call("resolve", json!(["127.0.0.1"]))),
            json!(["127.0.0.1"]),
            "{name}: the Store keeps serving"
        );

        // Pending accept: the listener closes, so a later client is refused.
        let (result, _) = cancel_after(&mut orb, "accept-pending", json!([port]));
        assert_eq!(ok(result), "end", "{name}: the pending accept ended");
        assert!(
            TcpStream::connect_timeout(
                &SocketAddr::from(([127, 0, 0, 1], port)),
                Duration::from_secs(1)
            )
            .is_err(),
            "{name}: the listener closed"
        );
        assert_eq!(
            ok(orb.call("resolve", json!(["127.0.0.1"]))),
            json!(["127.0.0.1"])
        );
        orb.close(&engine);
    }
}

/// Streams a cancelled call never read stay revision-owned: a retained receive stream survives
/// another call's cancellation and is read by a later call. A retained listen stream's accepted
/// but unread socket closes physically (its client sees the close) when the guest drops the
/// stream in a later call.
#[test]
#[ignore = "requires TEMBRA_WASM_SOCKETS"]
fn compiled_retained_streams_survive_cancellation_and_close_physically() {
    let Some(component) = component() else {
        return;
    };
    let artifact = artifact(&component);
    for (name, engine) in engines() {
        let kept = TcpListener::bind("127.0.0.1:0").unwrap();
        let kept_address = kept.local_addr().unwrap();
        let (idle, peer) = idle_peer();
        let port = free_port();
        let mut orb = Orb::install(
            &engine,
            &artifact,
            NetworkGrant {
                connect: vec![kept_address, idle],
                listen: vec![SocketAddr::from(([127, 0, 0, 1], port))],
                resolve: true,
                ..Default::default()
            },
        );
        ok(orb.call("retain-receive", json!([kept_address.port()])));
        let (mut kept_peer, _) = kept.accept().unwrap();
        // Another call is cancelled while the retained stream is untouched.
        let (result, _) = cancel_after(&mut orb, "read-pending", json!([idle.port()]));
        assert_eq!(ok(result), "end");
        assert!(peer.join().unwrap());
        kept_peer.write_all(b"kept").unwrap();
        assert_eq!(
            ok(orb.call("read-retained", json!([4]))),
            "kept",
            "{name}: the retained stream survived"
        );

        // A listen stream retained unread; a client is accepted into it by its pump.
        ok(orb.call("retain-listen", json!([port])));
        let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        // A later call gives the pump a Store turn, so the client is taken from upstream.
        ok(orb.call("resolve", json!(["127.0.0.1"])));
        orb.call("drop-retained", json!([])).unwrap();
        assert!(
            closed(&mut client),
            "{name}: the accepted, never delivered socket closed"
        );
        assert!(
            closed(&mut kept_peer),
            "{name}: the dropped receive stream and socket closed"
        );
        orb.close(&engine);
    }
}

/// A compiled send completes normally to a reading peer (all bytes, then end of stream). A call
/// blocked in a send to a peer that never reads (its kernel buffers full) is cancelled by its
/// caller: the guest's write returns at once, the call settles promptly, the send direction shuts
/// down (the peer drains what was queued, then sees end of stream) and the Store serves the next
/// call.
#[test]
#[ignore = "requires TEMBRA_WASM_SOCKETS"]
fn compiled_blocked_sends_end_with_their_cancelled_call() {
    let Some(component) = component() else {
        return;
    };
    let artifact = artifact(&component);
    for (name, engine) in engines() {
        let reading = TcpListener::bind("127.0.0.1:0").unwrap();
        let reading_address = reading.local_addr().unwrap();
        let receiver = thread::spawn(move || {
            let (mut socket, _) = reading.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut received = Vec::new();
            socket.read_to_end(&mut received).map(|_| received.len())
        });
        let silent = TcpListener::bind("127.0.0.1:0").unwrap();
        let silent_address = silent.local_addr().unwrap();
        let mut orb = Orb::install(
            &engine,
            &artifact,
            NetworkGrant {
                connect: vec![reading_address, silent_address],
                resolve: true,
                ..Default::default()
            },
        );
        // A normal send completes and ends the peer's stream.
        assert_eq!(
            ok(orb.call("send-blocked", json!([reading_address.port(), 3 << 20]))),
            "sent",
            "{name}"
        );
        assert_eq!(receiver.join().unwrap().unwrap(), 3 << 20, "{name}");

        // A send blocked by a peer that never reads: 16 MiB is far beyond what loopback kernel
        // buffers queue (about 2.6 MB), and its guest-side fill fits the default call fuel.
        let (context, finish) = orb.start("send-blocked", json!([silent_address.port(), 16 << 20]));
        let (mut peer, _) = silent.accept().unwrap();
        orb.pool
            .run_until(futures_timer::Delay::new(Duration::from_millis(500)));
        let cancelled = Instant::now();
        context.cancel().unwrap();
        let result = orb.pool.run_until(finish).unwrap();
        let settled = cancelled.elapsed();
        assert!(
            settled < Duration::from_secs(2),
            "{name}: settled {settled:?}"
        );
        let unsent = ok(result);
        assert!(
            unsent
                .as_str()
                .is_some_and(|text| text.starts_with("unsent ")),
            "{name}: the guest's write was released: {unsent}"
        );
        // The send's own future reports the cancellation, not a clean completion.
        assert!(
            unsent
                .as_str()
                .is_some_and(|text| text.contains("call cancelled")),
            "{name}: {unsent}"
        );
        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut drained = 0usize;
        let mut buffer = vec![0; 1 << 20];
        let ended = loop {
            match peer.read(&mut buffer) {
                Ok(0) => break true,
                Ok(count) => drained += count,
                Err(_) => break false,
            }
        };
        assert!(
            ended,
            "{name}: the send direction shut down after {drained} queued bytes"
        );
        assert_eq!(
            ok(orb.call("resolve", json!(["127.0.0.1"]))),
            json!(["127.0.0.1"]),
            "{name}: the Store keeps serving"
        );
        orb.close(&engine);
    }
}

/// A retained receive stream read by a call that is then cancelled ends for the guest, and its
/// completion future reports `other("call cancelled")`, never a clean truncated success. A real
/// end of stream and an explicit DROP keep upstream's own `ok`.
#[test]
#[ignore = "requires TEMBRA_WASM_SOCKETS"]
fn compiled_cancelled_reads_report_the_cancellation() {
    let Some(component) = component() else {
        return;
    };
    let artifact = artifact(&component);
    for (name, engine) in engines() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let mut orb = Orb::install(
            &engine,
            &artifact,
            NetworkGrant {
                connect: vec![address],
                resolve: true,
                ..Default::default()
            },
        );

        // Cancelled after a partial read.
        ok(orb.call("retain-receive", json!([address.port()])));
        let (mut peer, _) = listener.accept().unwrap();
        peer.write_all(b"abc").unwrap();
        let (partial, _) = cancel_after(&mut orb, "read-retained", json!([100]));
        assert_eq!(ok(partial), "abc", "{name}");
        let completion = ok(orb.call("retained-completion", json!([false])));
        assert!(
            completion
                .as_str()
                .is_some_and(|text| text.contains("call cancelled")),
            "{name}: {completion}"
        );
        assert!(closed(&mut peer), "{name}: the cancelled stream closed");

        // A real end of stream.
        ok(orb.call("retain-receive", json!([address.port()])));
        let (mut peer, _) = listener.accept().unwrap();
        peer.write_all(b"x").unwrap();
        peer.shutdown(std::net::Shutdown::Write).unwrap();
        assert_eq!(ok(orb.call("read-retained", json!([100]))), "x", "{name}");
        assert_eq!(
            ok(orb.call("retained-completion", json!([false]))),
            "ok",
            "{name}"
        );

        // An explicit DROP of the stream keeps upstream's policy.
        ok(orb.call("retain-receive", json!([address.port()])));
        let _peer = listener.accept().unwrap();
        assert_eq!(
            ok(orb.call("retained-completion", json!([true]))),
            "ok",
            "{name}"
        );
        orb.close(&engine);
    }
}

/// Socket streams opened while the component initializes (its `_initialize` creates a socket
/// and calls `receive` on it unconnected) take the adapter route like any other: the guest later
/// reads an empty stream and upstream's standard error on the completion future.
#[test]
#[ignore = "requires TEMBRA_WASM_SOCKETS"]
fn compiled_initializer_socket_streams_use_the_adapter() {
    let Some(component) = component() else {
        return;
    };
    let artifact = artifact(&component);
    for (name, engine) in engines() {
        let mut orb = Orb::install(
            &engine,
            &artifact,
            NetworkGrant {
                connect: vec!["127.0.0.1:9".parse().unwrap()],
                ..Default::default()
            },
        );
        let observed = ok(orb.call("initialized", json!([])));
        let observed = observed.as_str().unwrap_or_default().to_owned();
        assert!(
            observed.starts_with("read=0 status=end completion=Err("),
            "{name}: {observed}"
        );
        orb.close(&engine);
    }
}
