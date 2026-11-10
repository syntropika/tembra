//! Qualify standard `wasi:http` on the source path with raw v7 packets in the guest layout:
//! claims and transfers, writable futures, store drops, grants, the physical transport (DNS
//! jobs, connect, TLS, connection drivers) and its acknowledgements, against local peers.
use super::*;
use crate::http::Transport;
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::mpsc,
    thread,
    time::Duration,
};

const TYPES: &str = "wasi:http/types@0.3.0";
const CLIENT: &str = "wasi:http/client@0.3.0";
/// `future<result<option<trailers>, error-code>>`, the trailers input of `request.new`.
const TRAILERS: u32 = WRITABLE_FUTURES[0];
/// `future<result<_, error-code>>`, the `res` input of `consume-body`.
const RESULT: u32 = WRITABLE_FUTURES[1];
/// `error-code` cases used below.
const DENIED: u32 = 15;
const URI_INVALID: u32 = 19;
const READ_TIMEOUT: u32 = 9;

/// A raw request in the guest's layout; methods are found by interface and name.
struct P(Vec<u8>);

impl P {
    fn op(operation: u8) -> Self {
        Self(vec![PACKET_TAG, operation])
    }
    fn call(interface: &str, name: &str) -> Self {
        let index = (0..METHOD_NAMES.len())
            .find(|i| INTERFACES[*i] == interface && METHOD_NAMES[*i] == name)
            .unwrap_or_else(|| panic!("{interface}#{name} is not generated"));
        Self::op(CALL).u32(index as u32)
    }
    fn u8(mut self, value: u8) -> Self {
        self.0.push(value);
        self
    }
    fn u32(mut self, value: u32) -> Self {
        self.0.extend_from_slice(&value.to_le_bytes());
        self
    }
    fn u64(mut self, value: u64) -> Self {
        self.0.extend_from_slice(&value.to_le_bytes());
        self
    }
    fn bytes(mut self, value: &[u8]) -> Self {
        self.0
            .extend_from_slice(&(value.len() as u32).to_le_bytes());
        self.0.extend_from_slice(value);
        self
    }
}

type Access = wasmtime::component::Accessor<State>;

fn grant(origin: &str) -> crate::Grants {
    crate::Grants {
        http: vec![tembra_contracts::HttpGrant {
            provider: "web".into(),
            origins: vec![origin.into()],
        }],
        ..Default::default()
    }
}

fn store(grants: crate::Grants, transport: Transport) -> wasmtime::Store<State> {
    let engine = crate::Engine::new(crate::Limits::default()).unwrap();
    let mut state = super::super::runtime::test_state(wasmtime_wasi::WasiCtx::builder().build());
    state.standard.allowed.insert(TYPES.into());
    state.standard.allowed.insert(CLIENT.into());
    state.grant_http(grants, transport);
    wasmtime::Store::new(&engine.engine, state)
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn sync(access: &Access, packet: &P) -> Result<Vec<u8>, String> {
    access.with(|mut store| synchronous(store.as_context_mut(), &packet.0).unwrap())
}

fn id(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes[..8].try_into().unwrap())
}

/// Admit one async packet, keeping its task in `tasks` (the callback owner).
fn admit(access: &Access, packet: &P, tasks: &mut Tasks) -> Completion {
    access.with(|mut store| {
        let (task, leases, receive) = spawn(store.as_context_mut(), &packet.0).unwrap().unwrap();
        tasks.push(task, leases);
        receive
    })
}

async fn call(access: &Access, packet: &P, tasks: &mut Tasks) -> Result<Vec<u8>, String> {
    admit(access, packet, tasks)
        .await
        .unwrap()
        .map(|delivery| delivery.bytes)
}

fn fields(access: &Access) -> u64 {
    id(&sync(access, &P::call(TYPES, "[constructor]fields")).unwrap())
}

fn append(access: &Access, fields: u64, name: &str, value: &[u8]) -> Vec<u8> {
    sync(
        access,
        &P::call(TYPES, "[method]fields.append")
            .u64(fields)
            .bytes(name.as_bytes())
            .bytes(value),
    )
    .unwrap()
}

fn get(access: &Access, fields: u64, name: &str) -> Vec<Vec<u8>> {
    let reply = sync(
        access,
        &P::call(TYPES, "[method]fields.get")
            .u64(fields)
            .bytes(name.as_bytes()),
    )
    .unwrap();
    let mut reader = Reader(&reply);
    let count = reader.u32().unwrap();
    let values = (0..count)
        .map(|_| reader.bytes().unwrap().to_vec())
        .collect();
    reader.end().unwrap();
    values
}

fn future_new(access: &Access, ty: u32) -> Result<u64, String> {
    sync(access, &P::op(FUTURE_NEW).u32(ty)).map(|reply| id(&reply))
}

fn stream_new(access: &Access) -> u64 {
    id(&sync(access, &P::op(STREAM_NEW).u32(BYTE_STREAM)).unwrap())
}

/// `request.new` then method, scheme, authority and path; returns the request and its
/// transmission-result future.
fn request(
    access: &Access,
    body: Option<u64>,
    trailers: u64,
    (method, authority, path): (u32, &str, &str),
) -> (u64, u64) {
    let headers = fields(access);
    // hyper sends only request trailers declared by `Trailer`.
    assert_eq!(append(access, headers, "trailer", b"x-sum"), [0]);
    let packet = P::call(TYPES, "[static]request.new").u64(headers);
    let packet = match body {
        Some(body) => packet.u8(1).u64(body),
        None => packet.u8(0),
    };
    let reply = sync(access, &packet.u64(trailers).u8(0)).unwrap();
    let (request, sent) = (id(&reply), id(&reply[8..]));
    target(access, request, authority, path);
    assert_eq!(
        sync(
            access,
            &P::call(TYPES, "[method]request.set-method")
                .u64(request)
                .u32(method)
        )
        .unwrap(),
        [0]
    );
    (request, sent)
}

/// Set an `http` scheme, authority and path on a request.
fn target(access: &Access, request: u64, authority: &str, path: &str) {
    let set = |name: &str| P::call(TYPES, name).u64(request).u8(1);
    let scheme = set("[method]request.set-scheme").u32(0);
    let authority = set("[method]request.set-authority").bytes(authority.as_bytes());
    let path = set("[method]request.set-path-with-query").bytes(path.as_bytes());
    for packet in [scheme, authority, path] {
        assert_eq!(sync(access, &packet).unwrap(), [0]);
    }
}

/// An empty request body whose trailers future is already written `ok(none)`.
fn empty_request(access: &Access, authority: &str, path: &str) -> u64 {
    let trailers = future_new(access, TRAILERS).unwrap();
    sync(access, &P::op(FUTURE_WRITE).u64(trailers).u8(0).u8(0)).unwrap();
    request(access, None, trailers, (0, authority, path)).0
}

/// `client.send`: `Ok(response)` or `Err(error-code case)`.
async fn send(access: &Access, request: u64, tasks: &mut Tasks) -> Result<u64, u32> {
    let reply = call(access, &P::call(CLIENT, "send").u64(request), tasks)
        .await
        .unwrap();
    match reply[0] {
        0 => Ok(id(&reply[1..])),
        _ => Err(u32::from_le_bytes(reply[1..5].try_into().unwrap())),
    }
}

/// `consume-body` with a `res` future; returns the body stream and the trailers future.
fn consume(access: &Access, response: u64) -> (u64, u64) {
    let res = future_new(access, RESULT).unwrap();
    let reply = sync(
        access,
        &P::call(TYPES, "[static]response.consume-body")
            .u64(response)
            .u64(res),
    )
    .unwrap();
    (id(&reply), id(&reply[8..]))
}

/// One STREAM_READ: `Some(chunk)` or `None` at end of stream.
async fn read(access: &Access, stream: u64, tasks: &mut Tasks) -> Option<Vec<u8>> {
    let reply = call(access, &P::op(STREAM_READ).u64(stream).u32(65536), tasks)
        .await
        .unwrap();
    (reply[0] == 0).then(|| reply[5..].to_vec())
}

fn entries(access: &Access) -> usize {
    access.with(|mut store| store.data_mut().standard.handles.entries.len())
}

/// Read a request head.
fn head(reader: &mut BufReader<TcpStream>) -> String {
    let mut head = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap() == 0 {
            return head;
        }
        head += &line;
        if line == "\r\n" {
            return head;
        }
    }
}

/// Decode a chunked body: total bytes, trailer lines, and whether the terminating chunk came.
fn chunked(reader: &mut BufReader<TcpStream>) -> (usize, Vec<String>, bool) {
    let mut total = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return (total, Vec::new(), false);
        }
        let size = usize::from_str_radix(line.trim().split(';').next().unwrap(), 16).unwrap();
        if size == 0 {
            let mut trailers = Vec::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    return (total, trailers, false);
                }
                if line == "\r\n" {
                    return (total, trailers, true);
                }
                trailers.push(line.trim().to_owned());
            }
        }
        let mut chunk = vec![0; size + 2];
        if reader.read_exact(&mut chunk).is_err() {
            return (total, Vec::new(), false);
        }
        total += size;
    }
}

/// A one-connection peer; `script` gets the accepted socket and reports what it observed.
fn peer(
    script: impl FnOnce(TcpStream) -> String + Send + 'static,
) -> (SocketAddr, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (report, observed) = mpsc::channel();
    listener.set_nonblocking(true).unwrap();
    thread::spawn(move || {
        // A send aborted before connecting never arrives; report that instead of blocking.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let socket = loop {
            match listener.accept() {
                Ok((socket, _)) => break socket,
                Err(_) if std::time::Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(2));
                }
                Err(_) => {
                    let _ = report.send("unconnected".to_owned());
                    return;
                }
            }
        };
        socket.set_nonblocking(false).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        let _ = report.send(script(socket));
    });
    (address, observed)
}

#[test]
fn canonical_targets_are_checked_and_sent_exactly() {
    let _serial = serial();
    use crate::http::{Host, Target};
    let parse = |uri: &str| {
        uri.parse::<http::Uri>()
            .map_err(|error| error.to_string())
            .and_then(|uri| Target::parse(&uri))
    };
    let target = parse("https://Example.COM/a?b").unwrap();
    assert_eq!(target.host, Host::Name("example.com".into()));
    assert_eq!(
        (target.port, target.authority()),
        (443, "example.com".into())
    );
    assert_eq!(target.origin(), "https://example.com:443/");
    assert_eq!(target.path.as_str(), "/a?b");
    let target = parse("http://[::1]:8080/").unwrap();
    assert_eq!(target.authority(), "[::1]:8080");
    assert_eq!(
        parse("http://127.0.0.1:81/").unwrap().authority(),
        "127.0.0.1:81"
    );
    // Anything a URL parser would read differently from the raw authority is refused.
    for refused in [
        "http://127.1/",
        "http://0x7f.0.0.1/",
        "http://2130706433/",
        "http://a.0x10/",
        "http://user@example.com/",
        "http://ex%61mple.com/",
        "http://ex_ample.com/",
        "http://-a.com/",
        "http://example.com:0/",
        "ftp://example.com/",
    ] {
        assert!(parse(refused).is_err(), "{refused} was accepted");
    }
    // The grant is checked on the canonical origin only.
    let grants = grant("https://example.com");
    let target = parse("https://EXAMPLE.com:443/x").unwrap();
    crate::providers::check_http_origin(&grants, &target.origin(), &target.host_text()).unwrap();
    let other = parse("https://example.com:8443/").unwrap();
    assert!(
        crate::providers::check_http_origin(&grants, &other.origin(), &other.host_text()).is_err()
    );
}

#[test]
fn fields_claims_store_drops_and_writable_futures_follow_the_published_rules() {
    let _serial = serial();
    let runtime = runtime();
    let mut store = store(Default::default(), Transport::default());
    runtime
        .block_on(store.run_concurrent(async |access| {
            // Raw duplicate fields keep order; forbidden and invalid names are WIT errors.
            let headers = fields(access);
            assert_eq!(append(access, headers, "x-a", b"1"), [0]);
            assert_eq!(append(access, headers, "X-A", b"2"), [0]);
            assert_eq!(get(access, headers, "x-a"), [b"1".to_vec(), b"2".to_vec()]);
            assert_eq!(append(access, headers, "connection", b"x"), [1, 1, 0, 0, 0]);
            assert_eq!(append(access, headers, "bad name", b"x"), [1, 0, 0, 0, 0]);

            // Writable futures: unknown types are refused; writes are one-shot.
            assert!(future_new(access, BYTE_STREAM).is_err());
            let trailers = future_new(access, TRAILERS).unwrap();
            // A refused packet (trailing byte) transfers nothing.
            let base = P::call(TYPES, "[static]request.new")
                .u64(headers)
                .u8(0)
                .u64(trailers);
            assert!(sync(access, &P(base.0.clone()).u8(0).u8(9)).is_err());
            assert_eq!(get(access, headers, "x-a").len(), 2, "headers stayed owned");
            let reply = sync(access, &P(base.0.clone()).u8(0)).unwrap();
            let (request, sent) = (id(&reply), id(&reply[8..]));
            // Committed: headers and the trailers reader moved to upstream.
            assert!(
                sync(
                    access,
                    &P::call(TYPES, "[method]fields.has")
                        .u64(headers)
                        .bytes(b"x")
                )
                .is_err()
            );
            assert!(sync(access, &P(base.0).u8(0)).is_err());
            // FUTURE_WRITE carrying an owned handle transfers it only when accepted.
            let carried = fields(access);
            assert!(
                sync(
                    access,
                    &P::op(FUTURE_WRITE)
                        .u64(trailers)
                        .u8(0)
                        .u8(1)
                        .u64(carried)
                        .u8(0)
                )
                .is_err()
            );
            assert_eq!(
                get(access, carried, "x").len(),
                0,
                "refused write kept the handle"
            );
            sync(
                access,
                &P::op(FUTURE_WRITE).u64(trailers).u8(0).u8(1).u64(carried),
            )
            .unwrap();
            assert!(
                sync(
                    access,
                    &P::call(TYPES, "[method]fields.has")
                        .u64(carried)
                        .bytes(b"x")
                )
                .is_err()
            );
            assert!(
                sync(access, &P::op(FUTURE_WRITE).u64(trailers).u8(0).u8(0)).is_err(),
                "one-shot"
            );

            // Request options round-trip upstream semantics.
            let options =
                id(&sync(access, &P::call(TYPES, "[constructor]request-options")).unwrap());
            assert_eq!(
                sync(
                    access,
                    &P::call(TYPES, "[method]request-options.set-connect-timeout")
                        .u64(options)
                        .u8(1)
                        .u64(5_000_000_000)
                )
                .unwrap(),
                [0]
            );
            let reply = sync(
                access,
                &P::call(TYPES, "[method]request-options.get-connect-timeout").u64(options),
            )
            .unwrap();
            assert_eq!(reply, P::op(0).u8(1).u64(5_000_000_000).0[2..]);

            // Store-drop resources close through the queue in the same packet.
            sync(access, &P::op(DROP).u64(request)).unwrap();
            sync(access, &P::op(DROP).u64(sent)).unwrap();
            sync(access, &P::op(DROP).u64(options)).unwrap();
            access.with(|mut store| {
                assert!(
                    store
                        .data_mut()
                        .standard
                        .handles
                        .closing
                        .lock()
                        .unwrap()
                        .is_empty()
                )
            });
            assert!(sync(access, &P::op(DROP).u64(request)).is_err());
        }))
        .unwrap();
    assert!(store.data_mut().standard.check_trap().is_ok());
}

impl Standard {
    fn check_trap(&mut self) -> Result<(), String> {
        self.trapped.take().map_or(Ok(()), Err)
    }
}

#[test]
fn ungranted_and_non_canonical_targets_are_denied_before_any_io() {
    let _serial = serial();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let runtime = runtime();
    let transport = Transport::default();
    // Only `http://127.0.0.1:<port>` is granted; raw socket grants never authorize HTTP.
    let mut grants = grant(&format!("http://127.0.0.1:{port}"));
    grants.network = vec![tembra_contracts::NetworkGrant {
        provider: "raw".into(),
        connect: [format!("127.0.0.1:{}", port + 1).parse().unwrap()].into(),
        ..Default::default()
    }];
    let mut store = store(grants, transport.clone());
    runtime
        .block_on(store.run_concurrent(async |access| {
            let mut tasks = Tasks::default();
            // Typed refusals: a non-canonical target is an invalid URI; a missing grant is a
            // denial (a raw socket grant authorizes nothing here).
            for (authority, code) in [
                (format!("127.1:{port}"), URI_INVALID),
                (format!("127.0.0.1:{}", port + 1), DENIED),
                (format!("unresolvable.invalid:{port}"), DENIED),
            ] {
                let request = empty_request(access, &authority, "/");
                assert_eq!(
                    send(access, request, &mut tasks).await,
                    Err(code),
                    "{authority}"
                );
                assert_eq!(
                    transport.store_work.live(),
                    0,
                    "no resolver or connect job started"
                );
            }
        }))
        .unwrap();
    listener.set_nonblocking(true).unwrap();
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "no connection reached the granted peer"
    );
}

/// A streamed 3 MiB upload with request trailers, then a response whose first event arrives
/// before EOF and whose trailers are read through a handle-bearing future.
#[test]
fn streamed_upload_trailers_sse_and_response_trailers() {
    let _serial = serial();
    let (go, wait) = mpsc::channel::<()>();
    let (address, observed) = peer(move |socket| {
        let mut reader = BufReader::new(socket.try_clone().unwrap());
        let head = head(&mut reader);
        let (total, trailers, complete) = chunked(&mut reader);
        let mut socket = socket;
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nTrailer: x-done\r\n\r\nb\r\ndata: one\n\n\r\n")
            .unwrap();
        // Hold EOF until the guest has read the first event.
        wait.recv().unwrap();
        socket.write_all(b"0\r\nx-done: yes\r\n\r\n").unwrap();
        format!("{head}|{total}|{trailers:?}|{complete}")
    });
    let runtime = runtime();
    let transport = Transport::default();
    let mut store = store(grant(&format!("http://{address}")), transport.clone());
    runtime
        .block_on(store.run_concurrent(async |access| {
            let mut tasks = Tasks::default();
            let body = stream_new(access);
            let trailers = future_new(access, TRAILERS).unwrap();
            let (request, _) = request(
                access,
                Some(body),
                trailers,
                (2, &address.to_string(), "/upload?x=1"),
            );
            let sent = admit(access, &P::call(CLIENT, "send").u64(request), &mut tasks);
            let chunk = vec![7u8; 65536];
            for _ in 0..48 {
                call(
                    access,
                    &P::op(STREAM_WRITE).u64(body).bytes(&chunk),
                    &mut tasks,
                )
                .await
                .unwrap();
            }
            sync(access, &P::op(DROP).u64(body)).unwrap();
            let sum = fields(access);
            assert_eq!(append(access, sum, "x-sum", b"3145728"), [0]);
            sync(
                access,
                &P::op(FUTURE_WRITE).u64(trailers).u8(0).u8(1).u64(sum),
            )
            .unwrap();
            let reply = sent.await.unwrap().unwrap().bytes;
            assert_eq!(reply[0], 0, "send succeeded");
            let response = id(&reply[1..]);
            let status = sync(
                access,
                &P::call(TYPES, "[method]response.get-status-code").u64(response),
            )
            .unwrap();
            assert_eq!(status, 200u16.to_le_bytes());
            let (stream, trailers) = consume(access, response);
            assert_eq!(
                read(access, stream, &mut tasks).await.unwrap(),
                b"data: one\n\n"
            );
            go.send(()).unwrap();
            while read(access, stream, &mut tasks).await.is_some() {}
            // `ok(some(trailers))` issues a fields handle with the FUTURE_READ reply.
            let reply = call(access, &P::op(FUTURE_READ).u64(trailers), &mut tasks)
                .await
                .unwrap();
            assert_eq!(&reply[..2], [0, 1]);
            assert_eq!(get(access, id(&reply[2..]), "x-done"), [b"yes".to_vec()]);
        }))
        .unwrap();
    let observed = observed.recv_timeout(Duration::from_secs(30)).unwrap();
    assert!(
        observed.starts_with("POST /upload?x=1 HTTP/1.1\r\n"),
        "{observed}"
    );
    assert!(
        observed
            .to_ascii_lowercase()
            .contains(&format!("host: {address}\r\n")),
        "{observed}"
    );
    assert!(
        observed.ends_with("|3145728|[\"x-sum: 3145728\"]|true"),
        "{observed}"
    );
    // The callback-owned resolver/connect jobs are gone; the idle driver is a revision resource.
    assert_eq!(transport.store_work.live(), 0);
    drop(store);
    runtime.block_on(transport.drivers.settled());
}

/// An unwritten trailers future — dropped by the guest, or abandoned by callback retire —
/// fails the body: the peer never sees a terminating chunk.
#[test]
fn abandoned_trailers_never_commit_a_truncated_upload() {
    let _serial = serial();
    for retire_callback in [false, true] {
        let (address, observed) = peer(|socket| {
            let mut reader = BufReader::new(socket);
            head(&mut reader);
            let (total, _, complete) = chunked(&mut reader);
            format!("{total}|{complete}")
        });
        let runtime = runtime();
        let transport = Transport::default();
        let mut store = store(grant(&format!("http://{address}")), transport.clone());
        let mut tasks = Tasks::default();
        runtime
            .block_on(store.run_concurrent(async |access| {
                let body = stream_new(access);
                let trailers = future_new(access, TRAILERS).unwrap();
                let (request, _) =
                    request(access, Some(body), trailers, (2, &address.to_string(), "/"));
                let sent = admit(access, &P::call(CLIENT, "send").u64(request), &mut tasks);
                call(
                    access,
                    &P::op(STREAM_WRITE).u64(body).bytes(b"partial"),
                    &mut tasks,
                )
                .await
                .unwrap();
                sync(access, &P::op(DROP).u64(body)).unwrap();
                if !retire_callback {
                    sync(access, &P::op(DROP).u64(trailers)).unwrap();
                    let reply = sent.await.unwrap().unwrap().bytes;
                    assert_eq!(reply[0], 1, "send failed");
                }
            }))
            .unwrap();
        if retire_callback {
            runtime.block_on(retire(&mut store, &mut tasks)).unwrap();
        }
        // Either the body was cut without a terminating chunk, or (retire before the connect
        // finished) no connection was ever made; never a committed upload.
        let observed = observed.recv_timeout(Duration::from_secs(30)).unwrap();
        assert!(
            observed.ends_with("|false") || (retire_callback && observed == "unconnected"),
            "{observed}"
        );
        assert_eq!(transport.store_work.live(), 0);
    }
}

/// Request options keep upstream timeout semantics.
#[test]
fn first_byte_timeout_is_an_error_code() {
    let _serial = serial();
    let (address, observed) = peer(|socket| {
        let mut reader = BufReader::new(socket);
        let head = head(&mut reader);
        // Never answer; the client times out and closes.
        let mut rest = Vec::new();
        let _ = reader.read_to_end(&mut rest);
        head
    });
    let runtime = runtime();
    let mut store = store(grant(&format!("http://{address}")), Transport::default());
    runtime
        .block_on(store.run_concurrent(async |access| {
            let mut tasks = Tasks::default();
            // Options are attached at construction: a 100ms first-byte budget.
            let options =
                id(&sync(access, &P::call(TYPES, "[constructor]request-options")).unwrap());
            sync(
                access,
                &P::call(TYPES, "[method]request-options.set-first-byte-timeout")
                    .u64(options)
                    .u8(1)
                    .u64(100_000_000),
            )
            .unwrap();
            let trailers = future_new(access, TRAILERS).unwrap();
            sync(access, &P::op(FUTURE_WRITE).u64(trailers).u8(0).u8(0)).unwrap();
            let headers = fields(access);
            let reply = sync(
                access,
                &P::call(TYPES, "[static]request.new")
                    .u64(headers)
                    .u8(0)
                    .u64(trailers)
                    .u8(1)
                    .u64(options),
            )
            .unwrap();
            let timed = id(&reply);
            target(access, timed, &address.to_string(), "/slow");
            assert_eq!(send(access, timed, &mut tasks).await, Err(READ_TIMEOUT));
        }))
        .unwrap();
    assert!(
        observed
            .recv_timeout(Duration::from_secs(30))
            .unwrap()
            .starts_with("GET /slow ")
    );
}

/// Cancellation: a send aborted before its first poll leaves the request to upstream's store
/// drop; one aborted while waiting for the response closes the connection, and callback
/// retire returns only after its jobs physically ended.
#[test]
fn aborted_sends_close_requests_and_connections() {
    let _serial = serial();
    // Aborted before the first poll: no I/O at all, request closed by retire.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let runtime = runtime();
    let transport = Transport::default();
    let mut store = store(grant(&format!("http://{address}")), transport.clone());
    let mut tasks = Tasks::default();
    runtime
        .block_on(store.run_concurrent(async |access| {
            let request = empty_request(access, &address.to_string(), "/");
            let before = entries(access);
            access.with(|mut store| {
                let (task, leases, _) = spawn(
                    store.as_context_mut(),
                    &P::call(CLIENT, "send").u64(request).0,
                )
                .unwrap()
                .unwrap();
                tasks.push(task, leases);
            });
            // The request left the guest table at admission; the task owns it now.
            assert_eq!(entries(access), before - 1);
        }))
        .unwrap();
    runtime.block_on(retire(&mut store, &mut tasks)).unwrap();
    assert!(
        store
            .data()
            .standard
            .handles
            .closing
            .lock()
            .unwrap()
            .is_empty()
    );
    assert_eq!(transport.store_work.live(), 0);
    listener.set_nonblocking(true).unwrap();
    // Either never connected, or the aborted connect was dropped: no request bytes arrive.
    if let Ok((mut socket, _)) = listener.accept() {
        socket.set_nonblocking(false).unwrap();
        let mut bytes = Vec::new();
        socket.read_to_end(&mut bytes).unwrap();
        assert!(bytes.is_empty());
    }

    // Aborted while waiting for the response: the connection closes with the task.
    let (headed, wait) = mpsc::channel();
    let (address, observed) = peer(move |socket| {
        let mut reader = BufReader::new(socket);
        head(&mut reader);
        headed.send(()).unwrap();
        let mut rest = Vec::new();
        let _ = reader.read_to_end(&mut rest);
        "closed".to_owned()
    });
    let transport = Transport::default();
    let mut store = self::store(grant(&format!("http://{address}")), transport.clone());
    let mut tasks = Tasks::default();
    runtime
        .block_on(store.run_concurrent(async |access| {
            let request = empty_request(access, &address.to_string(), "/");
            let _sent = admit(access, &P::call(CLIENT, "send").u64(request), &mut tasks);
            while wait.try_recv().is_err() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }))
        .unwrap();
    runtime.block_on(retire(&mut store, &mut tasks)).unwrap();
    assert_eq!(
        observed.recv_timeout(Duration::from_secs(5)).unwrap(),
        "closed"
    );
    assert_eq!(
        (transport.store_work.live(), transport.drivers.live()),
        (0, 0)
    );
}

/// A retained, unconsumed response keeps its connection driver across callback retire (no
/// wait at every callback). A later callback consumes it; dropping the revision drops the
/// driver, which is the physical acknowledgement.
#[test]
fn retained_response_drivers_are_revision_resources() {
    let _serial = serial();
    let (address, observed) = peer(|socket| {
        let mut reader = BufReader::new(socket.try_clone().unwrap());
        head(&mut reader);
        let mut socket = socket;
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\none\r\n")
            .unwrap();
        let mut rest = Vec::new();
        let _ = reader.read_to_end(&mut rest);
        "closed".to_owned()
    });
    let runtime = runtime();
    let transport = Transport::default();
    // `localhost` exercises the tracked resolver.
    let authority = format!("localhost:{}", address.port());
    let mut store = store(grant(&format!("http://{authority}")), transport.clone());
    let mut tasks = Tasks::default();
    let mut response = 0;
    runtime
        .block_on(store.run_concurrent(async |access| {
            let request = empty_request(access, &authority, "/");
            response = send(access, request, &mut tasks).await.unwrap();
        }))
        .unwrap();
    runtime.block_on(retire(&mut store, &mut tasks)).unwrap();
    assert_eq!(
        (transport.store_work.live(), transport.drivers.live()),
        (0, 1)
    );
    assert!(
        observed.try_recv().is_err(),
        "the idle connection stays open"
    );
    // A later callback consumes the retained response.
    runtime
        .block_on(store.run_concurrent(async |access| {
            let (stream, _) = consume(access, response);
            assert_eq!(read(access, stream, &mut tasks).await.unwrap(), b"one");
        }))
        .unwrap();
    drop(store);
    runtime.block_on(transport.drivers.settled());
    assert_eq!(
        observed.recv_timeout(Duration::from_secs(5)).unwrap(),
        "closed"
    );
}

/// The resolver guard lives inside the blocking job: a caller that stops waiting (logical
/// cancellation) does not release it; only the physical return does. Capacity is bounded.
#[test]
fn resolver_jobs_are_acknowledged_only_when_the_lookup_returns() {
    let _serial = serial();
    let transport = Transport::default();
    let (release, blocked) = std::sync::mpsc::channel::<()>();
    let job = crate::io::dns_job(transport.store_work.hold(), move || {
        let _ = blocked.recv();
    })
    .unwrap();
    drop(job);
    assert_eq!(transport.store_work.live(), 1);
    // Admission is bounded: running plus queued jobs never exceed 64.
    let (stop, held) = std::sync::mpsc::channel::<()>();
    let held = std::sync::Arc::new(std::sync::Mutex::new(held));
    let mut admitted = 0;
    let mut refused = false;
    for _ in 0..80 {
        let held = std::sync::Arc::clone(&held);
        match crate::io::dns_job((), move || {
            let _ = held.lock().unwrap().recv_timeout(Duration::from_secs(10));
        }) {
            Ok(_) => admitted += 1,
            Err(_) => refused = true,
        }
    }
    assert!(refused && admitted < 64, "admitted {admitted}");
    drop(stop);
    release.send(()).unwrap();
    runtime().block_on(transport.store_work.settled());
}

/// Local HTTPS with a test-only trusted root; the default public roots refuse the same peer.
#[test]
fn local_https_verifies_the_canonical_host() {
    let _serial = serial();
    let (certificate, key, roots) = tls_fixture();
    for trusted in [true, false] {
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![certificate.clone()], key.clone_key())
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (report, observed) = mpsc::channel();
        thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            socket.set_nonblocking(true).unwrap();
            runtime().block_on(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let socket = tokio::net::TcpStream::from_std(socket).unwrap();
                let Ok(mut tls) = acceptor.accept(socket).await else {
                    let _ = report.send("handshake failed".to_owned());
                    return;
                };
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    tls.read_exact(&mut byte).await.unwrap();
                    head.push(byte[0]);
                }
                tls.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                    .await
                    .unwrap();
                tls.flush().await.unwrap();
                let _ = report.send(String::from_utf8(head).unwrap());
                // Keep the connection until the client closes it.
                let mut rest = Vec::new();
                let _ = tls.read_to_end(&mut rest).await;
            });
        });
        let transport = if trusted {
            Transport::trusting(roots.clone())
        } else {
            Transport::default()
        };
        let runtime = runtime();
        let mut store = store(grant(&format!("https://{address}")), transport);
        runtime
            .block_on(store.run_concurrent(async |access| {
                let mut tasks = Tasks::default();
                let trailers = future_new(access, TRAILERS).unwrap();
                sync(access, &P::op(FUTURE_WRITE).u64(trailers).u8(0).u8(0)).unwrap();
                let (request, _) =
                    request(access, None, trailers, (0, &address.to_string(), "/tls"));
                sync(
                    access,
                    &P::call(TYPES, "[method]request.set-scheme")
                        .u64(request)
                        .u8(1)
                        .u32(1),
                )
                .unwrap();
                let sent = send(access, request, &mut tasks).await;
                if trusted {
                    let Ok(sent) = sent else {
                        panic!(
                            "{sent:?}: peer {:?}",
                            observed.recv_timeout(Duration::from_secs(5))
                        );
                    };
                    let (stream, _) = consume(access, sent);
                    assert_eq!(read(access, stream, &mut tasks).await.unwrap(), b"ok");
                } else {
                    assert!(sent.is_err());
                }
            }))
            .unwrap();
        let observed = observed.recv_timeout(Duration::from_secs(10)).unwrap();
        if trusted {
            assert!(observed.starts_with("GET /tls HTTP/1.1\r\n"), "{observed}");
        } else {
            assert_eq!(observed, "handshake failed");
        }
    }
}

/// A test-only CA and a leaf certificate for the IP address `127.0.0.1` it issued.
fn tls_fixture() -> (
    rustls::pki_types::CertificateDer<'static>,
    rustls::pki_types::PrivateKeyDer<'static>,
    rustls::RootCertStore,
) {
    use rustls::pki_types::pem::PemObject;
    let certificate =
        rustls::pki_types::CertificateDer::from_pem_slice(TEST_CERTIFICATE.as_bytes()).unwrap();
    let key = rustls::pki_types::PrivateKeyDer::from_pem_slice(TEST_KEY.as_bytes()).unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from_pem_slice(TEST_CA.as_bytes()).unwrap())
        .unwrap();
    (certificate, key, roots)
}

const TEST_CERTIFICATE: &str = include_str!("http_qualification.crt");
const TEST_KEY: &str = include_str!("http_qualification.key");
const TEST_CA: &str = include_str!("http_qualification_ca.crt");

/// These tests share the process-wide reactor's bounded DNS pool; one saturates it on purpose.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A retained idle request whose trailers future was abandoned and whose `sent` future was
/// dropped keeps both endpoints counted (upstream still references them) but costs at most one
/// bounded settle window, once: later retirements return at once without polling the Store,
/// and the revision is not trapped. Dropping the request later releases both.
#[test]
fn retained_idle_requests_do_not_keep_retirement_polling() {
    let _serial = serial();
    let runtime = runtime();
    let mut store = store(Default::default(), Transport::default());
    let mut request = 0;
    runtime
        .block_on(store.run_concurrent(async |access| {
            let trailers = future_new(access, TRAILERS).unwrap();
            let (created, sent) = self::request(access, None, trailers, (0, "127.0.0.1:9", "/"));
            request = created;
            sync(access, &P::op(DROP).u64(trailers)).unwrap();
            sync(access, &P::op(DROP).u64(sent)).unwrap();
        }))
        .unwrap();
    let handles = &mut store.data_mut().standard.handles;
    let detached = |handles: &Handles| {
        handles
            .entries
            .values()
            .filter(|entry| matches!(entry, Entry::Detached(_)))
            .count()
    };
    assert_eq!(
        detached(handles),
        2,
        "upstream still references both endpoints"
    );
    // The abandoned trailers endpoint is passive from the start; the `sent` slot is polled for
    // in one bounded settle window, found idle, and never polled for again.
    assert_eq!(handles.unsettled(), Some(1));
    runtime
        .block_on(retire(&mut store, &mut Tasks::default()))
        .unwrap();
    let handles = &mut store.data_mut().standard.handles;
    assert_eq!(handles.unsettled(), None);
    assert!(!handles.purge());
    let started = std::time::Instant::now();
    for _ in 0..1000 {
        runtime
            .block_on(retire(&mut store, &mut Tasks::default()))
            .unwrap();
    }
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(detached(&store.data().standard.handles), 2);
    assert!(store.data_mut().standard.check_trap().is_ok());
    // A later callback drops the request through upstream's store drop; the endpoints finish.
    synchronous(store.as_context_mut(), &P::op(DROP).u64(request).0)
        .unwrap()
        .unwrap();
    // Upstream resolves and releases the `sent` slot over the Store's next polls.
    runtime
        .block_on(store.run_concurrent(async |_| {
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
        }))
        .unwrap();
    let handles = &mut store.data_mut().standard.handles;
    handles.purge();
    assert_eq!(detached(handles), 0);
    assert!(handles.entries.is_empty());
}

/// A guest closes an idle SSE response (no further data) by resolving the `res` future it gave
/// `consume-body` with `err` (the SDK's close), or by dropping it unwritten (abandonment):
/// upstream forwards that to the hooks' response-handling future, the driver closes the socket,
/// and callback retire returns only after it did. No producer or consumer poll is needed.
#[test]
fn closed_idle_response_closes_its_connection_before_retire_returns() {
    let _serial = serial();
    for abandon in [false, true] {
        let (address, observed) = peer(|socket| {
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            head(&mut reader);
            let mut socket = socket;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nd\r\ndata: hello\n\n\r\n")
                .unwrap();
            let mut rest = Vec::new();
            let _ = reader.read_to_end(&mut rest);
            "closed".to_owned()
        });
        let runtime = runtime();
        let transport = Transport::default();
        let mut store = store(grant(&format!("http://{address}")), transport.clone());
        let mut tasks = Tasks::default();
        runtime
            .block_on(store.run_concurrent(async |access| {
                let request = empty_request(access, &address.to_string(), "/events");
                let response = send(access, request, &mut tasks).await.unwrap();
                let res = future_new(access, RESULT).unwrap();
                let reply = sync(
                    access,
                    &P::call(TYPES, "[static]response.consume-body")
                        .u64(response)
                        .u64(res),
                )
                .unwrap();
                let (stream, trailers) = (id(&reply), id(&reply[8..]));
                assert_eq!(
                    read(access, stream, &mut tasks).await.unwrap(),
                    b"data: hello\n\n"
                );
                sync(access, &P::op(DROP).u64(stream)).unwrap();
                sync(access, &P::op(DROP).u64(trailers)).unwrap();
                if !abandon {
                    // err(internal-error(some("response closed"))), as the SDK's close().
                    let closed = P::op(FUTURE_WRITE)
                        .u64(res)
                        .u8(1)
                        .u32(38)
                        .u8(1)
                        .bytes(b"response closed");
                    sync(access, &closed).unwrap();
                }
                sync(access, &P::op(DROP).u64(res)).unwrap();
            }))
            .unwrap();
        runtime.block_on(retire(&mut store, &mut tasks)).unwrap();
        assert_eq!(
            (transport.store_work.live(), transport.drivers.live()),
            (0, 0),
            "the driver closed the socket before retire returned (abandon: {abandon})"
        );
        assert_eq!(
            observed.recv_timeout(Duration::from_secs(5)).unwrap(),
            "closed"
        );
        assert!(store.data_mut().standard.check_trap().is_ok());
    }
}

/// Issue `request.new` with an optional upload stream; returns the request and its `sent`
/// (transmission result) future.
fn post(access: &Access, body: Option<u64>, authority: &str, path: &str) -> (u64, u64) {
    let trailers = future_new(access, TRAILERS).unwrap();
    sync(access, &P::op(FUTURE_WRITE).u64(trailers).u8(0).u8(0)).unwrap();
    request(access, body, trailers, (2, authority, path))
}

/// `err(internal-error(some("response closed")))` written to a `res` future.
fn close_response(access: &Access, res: u64) {
    let closed = P::op(FUTURE_WRITE)
        .u64(res)
        .u8(1)
        .u32(38)
        .u8(1)
        .bytes(b"response closed");
    sync(access, &closed).unwrap();
}

/// F3 and F4 in isolation: the guest writes `err` to `res` while it still holds the body
/// stream; the next read ends without any DROP, the peer sees the close, and the request's
/// `sent` future of a still-uploading request reports an error, never a clean transmission.
#[test]
fn handling_error_closes_a_held_body_and_fails_the_upload() {
    let _serial = serial();
    let (headed, head_seen) = mpsc::channel();
    let (address, observed) = peer(move |socket| {
        let mut reader = BufReader::new(socket.try_clone().unwrap());
        head(&mut reader);
        let _ = headed.send(());
        // Answer at once, while the request body is still being uploaded.
        let mut socket = socket;
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nd\r\ndata: hello\n\n\r\n",
            )
            .unwrap();
        let mut rest = Vec::new();
        let _ = reader.read_to_end(&mut rest);
        "closed".to_owned()
    });
    let runtime = runtime();
    let transport = Transport::default();
    let mut store = store(grant(&format!("http://{address}")), transport.clone());
    let mut tasks = Tasks::default();
    runtime
        .block_on(store.run_concurrent(async |access| {
            let upload = stream_new(access);
            let (request, sent) = post(access, Some(upload), &address.to_string(), "/upload");
            let pending = admit(access, &P::call(CLIENT, "send").u64(request), &mut tasks);
            call(
                access,
                &P::op(STREAM_WRITE).u64(upload).bytes(b"partial"),
                &mut tasks,
            )
            .await
            .unwrap();
            while head_seen.try_recv().is_err() {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            let reply = pending.await.unwrap().unwrap().bytes;
            assert_eq!(reply[0], 0, "response head arrived during the upload");
            let response = id(&reply[1..]);
            let res = future_new(access, RESULT).unwrap();
            let reply = sync(
                access,
                &P::call(TYPES, "[static]response.consume-body")
                    .u64(response)
                    .u64(res),
            )
            .unwrap();
            let stream = id(&reply);
            assert_eq!(
                read(access, stream, &mut tasks).await.unwrap(),
                b"data: hello\n\n"
            );
            // Close through the handling result only; the stream is still held.
            close_response(access, res);
            assert_eq!(read(access, stream, &mut tasks).await, None, "body ended");
            let sent = call(access, &P::op(FUTURE_READ).u64(sent), &mut tasks)
                .await
                .unwrap();
            assert_eq!(
                sent[0], 1,
                "a cut upload never reports a clean transmission"
            );
        }))
        .unwrap();
    assert_eq!(
        observed.recv_timeout(Duration::from_secs(5)).unwrap(),
        "closed"
    );
    runtime.block_on(retire(&mut store, &mut tasks)).unwrap();
    assert_eq!(
        (transport.store_work.live(), transport.drivers.live()),
        (0, 0)
    );
}

/// F2 (source): a body the callback read and left unfinished is closed when the callback
/// retires, even if its `res` future was already resolved `ok`; retire returns only after the
/// close. An untouched response would survive (see `retained_response_drivers_...`).
#[test]
fn active_body_closes_with_its_callback() {
    let _serial = serial();
    let (address, observed) = peer(|socket| {
        let mut reader = BufReader::new(socket.try_clone().unwrap());
        head(&mut reader);
        let mut socket = socket;
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nd\r\ndata: hello\n\n\r\n",
            )
            .unwrap();
        let mut rest = Vec::new();
        let _ = reader.read_to_end(&mut rest);
        "closed".to_owned()
    });
    let runtime = runtime();
    let transport = Transport::default();
    let mut store = store(grant(&format!("http://{address}")), transport.clone());
    let mut tasks = Tasks::default();
    runtime
        .block_on(store.run_concurrent(async |access| {
            let request = empty_request(access, &address.to_string(), "/events");
            let response = send(access, request, &mut tasks).await.unwrap();
            let res = future_new(access, RESULT).unwrap();
            sync(access, &P::op(FUTURE_WRITE).u64(res).u8(0)).unwrap();
            let reply = sync(
                access,
                &P::call(TYPES, "[static]response.consume-body")
                    .u64(response)
                    .u64(res),
            )
            .unwrap();
            assert_eq!(
                read(access, id(&reply), &mut tasks).await.unwrap(),
                b"data: hello\n\n"
            );
        }))
        .unwrap();
    assert_eq!(transport.drivers.live(), 1, "open before the callback ends");
    runtime.block_on(retire(&mut store, &mut tasks)).unwrap();
    assert_eq!(
        (transport.store_work.live(), transport.drivers.live()),
        (0, 0),
        "retire returned after the close"
    );
    assert_eq!(
        observed.recv_timeout(Duration::from_secs(5)).unwrap(),
        "closed"
    );
}
