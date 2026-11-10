//! Source HTTP through the standard WASI HTTP sender, its grants and its callback lifetime.
//!
//! Peers write raw HTTP/1.1 bytes so each test controls framing and timing exactly.
use super::*;
use serde_json::json;
use std::io::{Read, Write};
use std::{
    net::{TcpListener, TcpStream},
    sync::mpsc,
    thread,
};

const WAIT: Duration = Duration::from_secs(5);

/// An installation granted exactly the HTTP `origins` and, separately, raw `sockets`.
fn installation(origins: &[String], sockets: &[SocketAddr]) -> SourceIo {
    let grants = crate::Grants {
        http: if origins.is_empty() {
            Vec::new()
        } else {
            vec![tembra_contracts::HttpGrant {
                provider: "web".into(),
                origins: origins.to_vec(),
            }]
        },
        ..Default::default()
    };
    SourceIo {
        builtins: Default::default(),
        destinations: Arc::new(sockets.iter().copied().collect()),
        grants: Arc::new(grants),
        streams: Streams::default(),
        standard: Arc::default(),
        timeout: Duration::from_secs(10),
    }
}

fn run(io: &SourceIo, packet: Value) -> Result<Value> {
    futures::executor::block_on(io.operation(&packet.to_string())?.execute())
}

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

fn get(url: &str) -> Value {
    json!({"operation":"http.send","url":url,"method":"GET","headers":[],"body":[]})
}

fn read(io: &SourceIo, id: u64) -> Result<Value> {
    run(io, json!({"operation":"http.read","id":id}))
}

/// Read a body to its end, recording the size of every chunk.
fn drain(io: &SourceIo, id: u64) -> (Vec<u8>, Vec<usize>) {
    let (mut bytes, mut chunks) = (Vec::new(), Vec::new());
    loop {
        match read(io, id).unwrap() {
            Value::Null => return (bytes, chunks),
            Value::Array(chunk) => {
                assert!(!chunk.is_empty() && chunk.len() <= TRANSPORT_CHUNK_BYTES);
                chunks.push(chunk.len());
                bytes.extend(chunk.iter().map(|byte| byte.as_u64().unwrap() as u8));
            }
            other => panic!("unexpected read {other}"),
        }
    }
}

/// Read one request head and its `Content-Length` body from a peer connection.
fn request(socket: &mut TcpStream) -> (String, Vec<u8>) {
    let mut head = Vec::new();
    let mut byte = [0];
    while !head.ends_with(b"\r\n\r\n") {
        socket.read_exact(&mut byte).unwrap();
        head.push(byte[0]);
    }
    let head = String::from_utf8(head).unwrap();
    let length = head
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length: ")
                .map(str::to_owned)
        })
        .map_or(0, |value| value.trim().parse().unwrap());
    let mut body = vec![0; length];
    socket.read_exact(&mut body).unwrap();
    (head, body)
}

/// Accept one connection on a fresh listener and run `script` on it; report when it closes.
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

fn origin(address: SocketAddr) -> String {
    format!("http://{address}")
}

/// A POST reaches the peer with the host's `Host` and length, and a chunked response arrives
/// in bounded reads through the standard stack.
#[test]
fn post_with_chunked_response() {
    let (address, closed) = peer(|socket| {
        let (head, body) = request(socket);
        assert!(head.starts_with("POST /submit?x=1 HTTP/1.1\r\n"), "{head}");
        assert!(
            head.to_ascii_lowercase()
                .contains(&"host: 127.0.0.1:".to_owned()),
            "{head}"
        );
        assert!(head.contains("x-trace: one\r\n"), "{head}");
        assert_eq!(body, b"payload");
        let large = vec![b'z'; 40_000];
        socket
            .write_all(b"HTTP/1.1 201 Created\r\nTransfer-Encoding: chunked\r\nX-Reply: ok\r\n\r\n")
            .unwrap();
        socket.write_all(b"5\r\nhello\r\n").unwrap();
        socket
            .write_all(format!("{:x}\r\n", large.len()).as_bytes())
            .unwrap();
        socket.write_all(&large).unwrap();
        socket.write_all(b"\r\n0\r\n\r\n").unwrap();
    });
    let io = installation(&[origin(address)], &[]);
    io.streams().begin_callback();
    let response = run(
        &io,
        json!({"operation":"http.send","url":format!("{}/submit?x=1", origin(address)),
            "method":"POST","headers":[["X-Trace","one"]],"body":b"payload".to_vec()}),
    )
    .unwrap();
    assert_eq!(response["status"], 201);
    assert!(
        response["headers"]
            .as_array()
            .unwrap()
            .contains(&json!(["x-reply", "ok"]))
    );
    let (bytes, chunks) = drain(&io, response["id"].as_u64().unwrap());
    assert_eq!(bytes.len(), 40_005);
    assert!(bytes.starts_with(b"hello") && bytes[5..].iter().all(|byte| *byte == b'z'));
    assert!(
        chunks.len() >= 3,
        "40005 bytes need at least three bounded reads"
    );
    io.streams().end_callback();
    closed.recv_timeout(WAIT).unwrap();
}

/// A server-sent event is readable while the peer keeps its connection open.
#[test]
fn server_sent_events_arrive_before_the_peer_ends() {
    let (release, released) = mpsc::channel::<()>();
    let (address, closed) = peer(move |socket| {
        request(socket);
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n")
            .unwrap();
        let event = b"data: first\n\n";
        socket
            .write_all(format!("{:x}\r\n", event.len()).as_bytes())
            .unwrap();
        socket.write_all(event).unwrap();
        socket.write_all(b"\r\n").unwrap();
        // Hold the stream open until the guest proves it has read the first event.
        released.recv_timeout(Duration::from_secs(10)).unwrap();
    });
    let io = installation(&[origin(address)], &[]);
    io.streams().begin_callback();
    let response = run(&io, get(&format!("{}/events", origin(address)))).unwrap();
    let id = response["id"].as_u64().unwrap();
    let first = read(&io, id).unwrap();
    let text: Vec<u8> = first
        .as_array()
        .unwrap()
        .iter()
        .map(|byte| byte.as_u64().unwrap() as u8)
        .collect();
    assert_eq!(text, b"data: first\n\n");
    // The next read waits for more data; closing the body settles it and drops the connection.
    let pending = start(&io, json!({"operation":"http.read","id":id}));
    assert!(pending.recv_timeout(Duration::from_millis(200)).is_err());
    run(&io, json!({"operation":"http.close","id":id})).unwrap();
    let error = pending.recv_timeout(WAIT).unwrap().unwrap_err();
    assert!(error.message.contains("closed"), "{error}");
    release.send(()).unwrap();
    closed
        .recv_timeout(WAIT)
        .expect("the connection closed with its body");
    // Closing again is idempotent for an issued identity.
    run(&io, json!({"operation":"http.close","id":id})).unwrap();
    io.streams().end_callback();
}

/// Ending the callback abandons a request awaiting its response and a pending body read.
#[test]
fn callback_end_cancels_requests_and_reads() {
    let (address, closed) = peer(|socket| {
        request(socket);
    });
    let io = installation(&[origin(address)], &[]);
    io.streams().begin_callback();
    let waiting = start(&io, get(&origin(address)));
    assert!(waiting.recv_timeout(Duration::from_millis(200)).is_err());
    let ended = Instant::now();
    io.streams().end_callback();
    let error = waiting.recv_timeout(WAIT).unwrap().unwrap_err();
    assert!(error.message.contains("callback ended"), "{error}");
    assert!(ended.elapsed() < Duration::from_secs(2));
    closed
        .recv_timeout(WAIT)
        .expect("the abandoned connection closed");

    let (release, released) = mpsc::channel::<()>();
    let (address, closed) = peer(move |socket| {
        request(socket);
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\npartial")
            .unwrap();
        let _ = released.recv_timeout(Duration::from_secs(10));
    });
    let io = installation(&[origin(address)], &[]);
    io.streams().begin_callback();
    let id = run(&io, get(&origin(address))).unwrap()["id"]
        .as_u64()
        .unwrap();
    assert_eq!(read(&io, id).unwrap().as_array().unwrap().len(), 7);
    let pending = start(&io, json!({"operation":"http.read","id":id}));
    io.streams().end_callback();
    let error = pending.recv_timeout(WAIT).unwrap().unwrap_err();
    assert!(error.message.contains("closed"), "{error}");
    release.send(()).unwrap();
    closed.recv_timeout(WAIT).unwrap();
    // The body of an ended callback cannot be selected by the next one.
    io.streams().begin_callback();
    assert!(read(&io, id).is_err());
    io.streams().end_callback();
}

/// Origins are exact grants; raw socket grants never authorize HTTP; handles do not cross kinds.
#[test]
fn grants_and_handles_are_enforced() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let refuse = |io: &SourceIo, url: &str| {
        io.streams().begin_callback();
        let error = run(io, get(url)).unwrap_err();
        io.streams().end_callback();
        error
    };
    // A raw socket grant to the same address does not admit HTTP.
    let error = refuse(&installation(&[], &[address]), &origin(address));
    assert!(error.message.contains("not granted"), "{error}");
    let granted = installation(&[origin(address)], &[]);
    for (url, reason) in [
        (format!("https://{address}/"), "not granted"),
        ("http://127.0.0.2:1/".to_owned(), "not granted"),
        (format!("http://user@{address}/"), "credentials"),
    ] {
        let error = refuse(&granted, &url);
        assert!(error.message.contains(reason), "{url}: {error}");
    }
    assert!(
        listener.accept().is_err(),
        "a refused request must not connect"
    );
    // Requests need an active callback and use only host-owned framing headers.
    assert!(run(&granted, get(&origin(address))).is_err());
    granted.streams().begin_callback();
    for header in ["Host", "Transfer-Encoding", "Content-Length", "Connection"] {
        let error = run(
            &granted,
            json!({"operation":"http.send","url":origin(address),"method":"GET",
                "headers":[[header,"x"]],"body":[]}),
        )
        .unwrap_err();
        assert!(
            error.message.contains("set by the host"),
            "{header}: {error}"
        );
    }
    let large = vec![0u8; crate::protocol::HTTP_BODY_BYTES + 1];
    let headers = vec![("x-test", "value"); crate::protocol::HTTP_HEADER_LIMIT + 1];
    assert!(run(&granted, json!({"operation":"http.send","url":origin(address),"method":"GET","headers":headers,"body":[]})).is_err());
    assert!(
        run(
            &granted,
            json!({"operation":"http.send","url":origin(address),"method":"POST","headers":[],"body":large}),
        )
        .is_err()
    );
    // Unissued identities are unknown.
    assert!(read(&granted, 1).is_err());
    assert!(run(&granted, json!({"operation":"http.close","id":7})).is_err());
    granted.streams().end_callback();
    // A raw socket identity never selects an HTTP body.
    let both = installation(&[origin(address)], &[address]);
    both.streams().begin_callback();
    let socket = run(
        &both,
        json!({"operation":"transport.connect","ip":"127.0.0.1","port":address.port()}),
    )
    .unwrap()
    .as_u64()
    .unwrap();
    let error = read(&both, socket).unwrap_err();
    assert!(
        error.message.contains("unknown HTTP response body"),
        "{error}"
    );
    run(&both, json!({"operation":"transport.close","id":socket})).unwrap();
    both.streams().end_callback();
}

/// Real HTTPS: DNS, a publicly trusted certificate and TLS through the unchanged upstream sender.
#[test]
#[ignore = "requires outbound HTTPS to example.com"]
fn public_https_uses_the_standard_sender() {
    let io = installation(&["https://example.com".into()], &[]);
    io.streams().begin_callback();
    let response = run(&io, get("https://example.com/")).unwrap();
    assert_eq!(response["status"], 200);
    let (body, _) = drain(&io, response["id"].as_u64().unwrap());
    assert!(String::from_utf8_lossy(&body).contains("Example Domain"));
    // The same origin over plain HTTP is a different, ungranted origin.
    assert!(run(&io, get("http://example.com/")).is_err());
    io.streams().end_callback();
}
