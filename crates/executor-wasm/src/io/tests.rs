use super::*;
use serde_json::json;
use std::io::{Read, Write};
use std::{net::TcpListener, sync::mpsc, thread};

const WAIT: Duration = Duration::from_secs(5);

/// An installation granted exactly `destinations`.
fn sockets(destinations: &[SocketAddr], _directory: Option<&std::path::Path>) -> SourceIo {
    SourceIo {
        builtins: Default::default(),
        destinations: Arc::new(destinations.iter().copied().collect()),
        grants: Arc::default(),
        streams: Streams::default(),
        standard: Arc::default(),
        timeout: Duration::from_secs(5),
    }
}

fn run(io: &SourceIo, packet: Value) -> Result<Value> {
    futures::executor::block_on(io.operation(&packet.to_string())?.execute())
}

/// Submit without waiting; the receiver observes the single completion.
fn start(io: &SourceIo, packet: Value) -> mpsc::Receiver<Result<Value>> {
    let (send, receive) = mpsc::channel();
    match io.operation(&packet.to_string()) {
        Ok(operation) => operation.submit(move |outcome| {
            let _ = send.send(outcome);
        }),
        Err(error) => send.send(Err(error)).unwrap(),
    }
    receive
}

fn connect(io: &SourceIo, address: SocketAddr) -> Result<u32> {
    run(
        io,
        json!({"operation":"transport.connect","ip":address.ip().to_string(),"port":address.port()}),
    )
    .map(|id| id.as_u64().unwrap() as u32)
}

fn read(io: &SourceIo, id: u32) -> Result<Value> {
    run(io, json!({"operation":"transport.read","id":id}))
}

/// A peer that accepts `count` connections and reports when each observes end of stream.
struct Peer {
    address: SocketAddr,
    closed: mpsc::Receiver<Vec<u8>>,
}

fn peer(count: usize, reply: Vec<u8>) -> Peer {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (send, closed) = mpsc::channel();
    thread::spawn(move || {
        for _ in 0..count {
            let (mut socket, _) = listener.accept().unwrap();
            let send = send.clone();
            let reply = reply.clone();
            thread::spawn(move || {
                socket.write_all(&reply).unwrap();
                if !reply.is_empty() {
                    socket.shutdown(std::net::Shutdown::Write).unwrap();
                }
                let mut received = Vec::new();
                // Returns once the host side is physically closed.
                let _ = socket.read_to_end(&mut received);
                let _ = send.send(received);
            });
        }
    });
    Peer { address, closed }
}

/// A response larger than one chunk arrives in bounded pieces and ends with `null`.
#[test]
fn reads_deliver_split_bytes_then_end_of_stream() {
    let body = (0..40_000u32).map(|index| index as u8).collect::<Vec<_>>();
    let peer = peer(1, body.clone());
    let io = sockets(&[peer.address], None);
    io.streams().begin_callback();
    let id = connect(&io, peer.address).unwrap();
    let mut received = Vec::new();
    let mut chunks = 0;
    loop {
        match read(&io, id).unwrap() {
            Value::Null => break,
            Value::Array(bytes) => {
                assert!(!bytes.is_empty() && bytes.len() <= TRANSPORT_CHUNK_BYTES);
                received.extend(bytes.iter().map(|byte| byte.as_u64().unwrap() as u8));
                chunks += 1;
            }
            other => panic!("unexpected read {other}"),
        }
    }
    assert_eq!(received, body);
    assert!(chunks >= 3, "40000 bytes need at least three bounded reads");
    assert_eq!(read(&io, id).unwrap(), Value::Null);
    io.streams().end_callback();
    peer.closed.recv_timeout(WAIT).unwrap();
}

/// Writes reach the peer; closing settles a pending read and the handle is never reissued.
#[test]
fn close_settles_pending_reads_and_handles_stay_unique() {
    let peer = peer(2, Vec::new());
    let io = sockets(&[peer.address], None);
    io.streams().begin_callback();
    let first = connect(&io, peer.address).unwrap();
    run(
        &io,
        json!({"operation":"transport.write","id":first,"bytes":b"hello".to_vec()}),
    )
    .unwrap();
    let oversized = vec![0u8; TRANSPORT_CHUNK_BYTES + 1];
    assert!(
        run(
            &io,
            json!({"operation":"transport.write","id":first,"bytes":oversized})
        )
        .is_err()
    );
    let pending = start(&io, json!({"operation":"transport.read","id":first}));
    let second_read = start(&io, json!({"operation":"transport.read","id":first}));
    let refused = second_read.recv_timeout(WAIT).unwrap().unwrap_err();
    assert!(refused.message.contains("already pending"), "{refused}");
    run(&io, json!({"operation":"transport.close","id":first})).unwrap();
    let error = pending.recv_timeout(WAIT).unwrap().unwrap_err();
    assert!(error.message.contains("closed"), "{error}");
    assert_eq!(peer.closed.recv_timeout(WAIT).unwrap(), b"hello");
    for packet in [
        json!({"operation":"transport.read","id":first}),
        json!({"operation":"transport.write","id":first,"bytes":[1]}),
    ] {
        let error = run(&io, packet).unwrap_err();
        assert!(
            error.message.contains("unknown transport stream"),
            "{error}"
        );
    }
    // Cleanup can race cancellation or be repeated, but cannot select an unissued identity.
    run(&io, json!({"operation":"transport.close","id":first})).unwrap();
    assert!(run(&io, json!({"operation":"transport.close","id":0})).is_err());
    assert!(run(&io, json!({"operation":"transport.close","id":first+1})).is_err());
    let second = connect(&io, peer.address).unwrap();
    assert_ne!(second, first);
    // A new callback cannot select a stream issued to the previous one.
    io.streams().end_callback();
    peer.closed.recv_timeout(WAIT).unwrap();
    io.streams().begin_callback();
    assert!(read(&io, second).is_err());
    run(&io, json!({"operation":"transport.close","id":second})).unwrap();
    io.streams().end_callback();
}

/// Only exact granted destinations connect, only during a callback, and at most 16 at once.
#[test]
fn grants_callbacks_and_stream_limits_are_enforced() {
    let peer = peer(17, Vec::new());
    let other = TcpListener::bind("127.0.0.1:0").unwrap();
    let io = sockets(&[peer.address], None);
    let error = connect(&io, peer.address).unwrap_err();
    assert!(error.message.contains("active callback"), "{error}");
    io.streams().begin_callback();
    let error = connect(&io, other.local_addr().unwrap()).unwrap_err();
    assert!(error.message.contains("not granted"), "{error}");
    let error = run(
        &io,
        json!({"operation":"transport.connect","ip":"localhost","port":peer.address.port()}),
    )
    .unwrap_err();
    assert!(error.message.contains("literal IP"), "{error}");
    let ids = (0..streams::STREAM_LIMIT)
        .map(|_| connect(&io, peer.address).unwrap())
        .collect::<Vec<_>>();
    let error = connect(&io, peer.address).unwrap_err();
    assert!(error.message.contains("limit"), "{error}");
    run(&io, json!({"operation":"transport.close","id":ids[0]})).unwrap();
    connect(&io, peer.address).unwrap();
    io.streams().end_callback();
    for _ in 0..=streams::STREAM_LIMIT {
        peer.closed.recv_timeout(WAIT).unwrap();
    }
}

/// A connection admitted by an ended callback never becomes a usable handle.
#[test]
fn late_connection_after_callback_end_is_discarded() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let io = sockets(&[address], None);
    io.streams().begin_callback();
    let admitted = io
        .operation(
            &json!({"operation":"transport.connect","ip":"127.0.0.1","port":address.port()})
                .to_string(),
        )
        .unwrap();
    io.streams().end_callback();
    io.streams().begin_callback();
    let error = futures::executor::block_on(admitted.execute()).unwrap_err();
    assert!(error.message.contains("callback ended"), "{error}");
    // If the kernel completed the handshake, the discarded socket is closed, not retained.
    listener.set_nonblocking(true).unwrap();
    if let Ok((mut socket, _)) = listener.accept() {
        socket.set_nonblocking(false).unwrap();
        socket.set_read_timeout(Some(WAIT)).unwrap();
        let mut rest = Vec::new();
        assert_eq!(socket.read_to_end(&mut rest).unwrap(), 0);
    }
    // No stream was registered: the first issued identity is still unused by the guest.
    assert!(read(&io, 1).is_err());
    io.streams().end_callback();
}
