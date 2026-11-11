//! A compiled Orb serves incoming HTTP through its exact `wasi:http/handler@0.3.0` export.
//!
//! The guest (`rust-http-handler`) returns its response at once and writes the body from a task
//! that keeps running in the same call. The application reads the body on its own thread, as a
//! server would, while the host's executor and the Orb's Store run independently.
use bytes::Bytes;
use futures::{
    FutureExt, StreamExt,
    channel::mpsc::{UnboundedSender, unbounded},
    executor::LocalPool,
};
use http_body_util::{BodyExt, StreamBody};
use std::{
    collections::BTreeMap,
    convert::Infallible,
    future::Future,
    io::{Read, Write},
    net::TcpListener,
    rc::Rc,
    sync::mpsc,
    thread,
    time::Duration,
};
use tembra::{Host, Reference};
use tembra_contracts::HttpGrant;
use tembra_executor_wasm::{
    Contract, Engine, Grants, Install, Limits,
    ingress::{ResponseBody, Settled, handler_service, serve},
};

const ORB: &str = "web";

fn fixture(variable: &str) -> Option<Vec<u8>> {
    let Some(path) = std::env::var_os(variable) else {
        eprintln!("skipped: {variable} names no compiled HTTP handler");
        return None;
    };
    Some(std::fs::read(path).unwrap())
}

fn artifact(component: &[u8], world: &str, api_version: u32) -> Vec<u8> {
    Contract::from_dir(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/wit/handler"),
        Some(world),
    )
    .unwrap()
    .component_artifact(component, ORB, api_version, &Default::default())
    .unwrap()
}

fn engines(limits: Limits) -> [Engine; 2] {
    [
        Engine::new(limits).unwrap().network("local").unwrap(),
        Engine::interpreted(limits)
            .unwrap()
            .network("local")
            .unwrap(),
    ]
}

/// One installed handler Orb and its host.
struct Served {
    pool: LocalPool,
    host: Host,
    reference: Rc<Reference>,
}

fn install(engine: &Engine, artifact: &[u8], install: Install) -> Served {
    let pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![engine.load(artifact, install).unwrap()])
        .unwrap();
    let mut served = Served {
        reference: Rc::new(
            host.get(&handler_service(ORB), vec!["main".into()], "alice")
                .unwrap(),
        ),
        pool,
        host,
    };
    served.pool.run_until(served.reference.ready()).unwrap();
    served
}

type RequestBody = StreamBody<
    futures::stream::Map<
        futures::channel::mpsc::UnboundedReceiver<Bytes>,
        fn(Bytes) -> Result<http_body::Frame<Bytes>, Infallible>,
    >,
>;

/// A request whose body the test writes chunk by chunk; dropping the sender ends it.
fn request(path: &str) -> (http::Request<RequestBody>, UnboundedSender<Bytes>) {
    let (sender, receiver) = unbounded();
    let frame: fn(Bytes) -> Result<http_body::Frame<Bytes>, Infallible> =
        |bytes| Ok(http_body::Frame::data(bytes));
    let request = http::Request::post(format!("http://ingress.test{path}"))
        .body(StreamBody::new(receiver.map(frame)))
        .unwrap();
    (request, sender)
}

#[derive(Debug, PartialEq)]
enum Frame {
    Data(String),
    Trailers(BTreeMap<String, String>),
    End,
    Error(String),
}

/// Reads a response body on its own thread and runtime, like a server connection.
struct Reader {
    commands: mpsc::Sender<bool>,
    frames: futures::channel::mpsc::UnboundedReceiver<Frame>,
}

impl Reader {
    fn new(body: ResponseBody) -> Self {
        let (commands, requests) = mpsc::channel::<bool>();
        let (frames, receive) = unbounded();
        thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let mut body = Some(body);
            // `true` reads the next frame; `false` drops the body (a client disconnect).
            while let Ok(next) = requests.recv() {
                if !next {
                    body = None;
                    continue;
                }
                let Some(reading) = body.as_mut() else {
                    let _ = frames.unbounded_send(Frame::End);
                    continue;
                };
                let frame = match runtime.block_on(reading.frame()) {
                    None => Frame::End,
                    Some(Err(error)) => Frame::Error(error.to_string()),
                    Some(Ok(frame)) => match frame.into_data() {
                        Ok(data) => Frame::Data(String::from_utf8(data.to_vec()).unwrap()),
                        Err(frame) => Frame::Trailers(
                            frame
                                .into_trailers()
                                .unwrap()
                                .iter()
                                .map(|(name, value)| {
                                    (name.to_string(), value.to_str().unwrap().to_owned())
                                })
                                .collect(),
                        ),
                    },
                };
                let _ = frames.unbounded_send(frame);
            }
        });
        Self {
            commands,
            frames: receive,
        }
    }

    /// The next frame, while the host's executor keeps running.
    fn next(&mut self, pool: &mut LocalPool) -> Frame {
        self.commands.send(true).unwrap();
        let frame = self.frames.next();
        let deadline = futures_timer::Delay::new(Duration::from_secs(10));
        match pool.run_until(futures::future::select(frame, deadline)) {
            futures::future::Either::Left((Some(frame), _)) => frame,
            _ => panic!("no response frame within the bound"),
        }
    }

    fn disconnect(&self) {
        self.commands.send(false).unwrap();
    }
}

fn data(text: &str) -> Frame {
    Frame::Data(format!("data: {text}\n\n"))
}

/// Serve one request and return its head, body reader and completion.
fn start(
    served: &mut Served,
    path: &str,
) -> (
    http::response::Parts,
    Reader,
    Settled,
    UnboundedSender<Bytes>,
) {
    let (request, sender) = request(path);
    let (response, settled) = served
        .pool
        .run_until(serve(
            &served.host,
            "alice",
            served.reference.clone(),
            request,
        ))
        .unwrap();
    let (parts, body) = response.into_parts();
    (parts, Reader::new(body), settled, sender)
}

/// Await completion within a bound, keeping the host running.
fn settle(pool: &mut LocalPool, settled: Settled) -> tembra::Result<()> {
    let deadline = futures_timer::Delay::new(Duration::from_secs(10));
    match pool.run_until(futures::future::select(settled, deadline)) {
        futures::future::Either::Left((result, _)) => result,
        futures::future::Either::Right(_) => panic!("the request did not settle"),
    }
}

/// The body-less `/status` response: how many requests this Store served and how the previous
/// producer ended. Its success proves the Store is still usable.
fn status(served: &mut Served) -> (u32, String) {
    let (parts, mut reader, settled, _sender) = start(served, "/status");
    assert_eq!(reader.next(&mut served.pool), Frame::End);
    settle(&mut served.pool, settled).unwrap();
    let header = |name: &str| parts.headers[name].to_str().unwrap().to_owned();
    (header("x-requests").parse().unwrap(), header("x-last-exit"))
}

fn close(mut served: Served, engine: &Engine) {
    served.reference.release().unwrap();
    served.pool.run_until(served.host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
}

fn pending(pool: &mut LocalPool, settled: &mut Settled) -> bool {
    pool.run_until_stalled();
    settled.now_or_never().is_none()
}

/// Headers and incremental events arrive before EOF; request chunks stream into the guest;
/// trailers follow the last event; the same Store then serves another request.
#[test]
#[ignore = "requires TEMBRA_WASM_HTTP_HANDLER"]
fn compiled_handlers_stream_headers_and_events_before_eof() {
    let Some(component) = fixture("TEMBRA_WASM_HTTP_HANDLER") else {
        return;
    };
    for engine in engines(Limits::default()) {
        // A handler-only Orb needs no grant at all.
        let mut served = install(
            &engine,
            &artifact(&component, "handler", 1),
            Install::default(),
        );
        let (parts, mut reader, mut settled, sender) = start(&mut served, "/sse");
        assert_eq!(parts.status, 200);
        assert_eq!(parts.headers["content-type"], "text/event-stream");
        assert!(pending(&mut served.pool, &mut settled));
        assert_eq!(reader.next(&mut served.pool), data("open"));
        assert!(pending(&mut served.pool, &mut settled));
        for chunk in ["alpha", "beta"] {
            sender.unbounded_send(Bytes::from(chunk)).unwrap();
            assert_eq!(reader.next(&mut served.pool), data(chunk));
            assert!(pending(&mut served.pool, &mut settled));
        }
        drop(sender);
        assert_eq!(reader.next(&mut served.pool), data("end"));
        assert_eq!(
            reader.next(&mut served.pool),
            Frame::Trailers([("x-events".to_owned(), "4".to_owned())].into())
        );
        assert_eq!(reader.next(&mut served.pool), Frame::End);
        settle(&mut served.pool, settled).unwrap();
        let (requests, exit) = status(&mut served);
        assert_eq!(requests, 2);
        assert!(exit.starts_with("complete events=4 transmit=Ok"), "{exit}");
        close(served, &engine);
    }
}

/// Serve one scripted HTTP response to a single connection.
fn upstream(body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut head = Vec::new();
        let mut byte = [0];
        while !head.ends_with(b"\r\n\r\n") {
            socket.read_exact(&mut byte).unwrap();
            head.push(byte[0]);
        }
        write!(
            socket,
            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
    });
    format!("http://{address}")
}

/// The request's root outlives the head: after the application took the headers and dropped
/// its completion observer, the guest still sends a granted outgoing request (whose pre-send
/// check requires the live root) and streams its result.
#[test]
#[ignore = "requires TEMBRA_WASM_HTTP_PROXY"]
fn handler_authority_outlives_the_returned_head() {
    let Some(component) = fixture("TEMBRA_WASM_HTTP_PROXY") else {
        return;
    };
    for engine in engines(Limits::default()) {
        let artifact = artifact(&component, "proxy", 1);
        // The outgoing client still requires an HTTP grant.
        let refused = engine.load(&artifact, Install::default()).err().unwrap();
        assert_eq!(refused.code, tembra::core::Code::Forbidden, "{refused}");
        let origin = upstream("hello");
        let mut install_with_grant = Install::default();
        install_with_grant.grants = Grants {
            http: vec![HttpGrant {
                provider: "local".into(),
                origins: vec![origin.clone()],
            }],
            ..Grants::default()
        };
        let mut served = install(&engine, &artifact, install_with_grant);
        let (_, mut reader, settled, sender) = start(&mut served, "/proxy");
        // Nothing waits for completion any more; the root must stay alive anyway.
        drop(settled);
        assert_eq!(reader.next(&mut served.pool), data("open"));
        sender
            .unbounded_send(Bytes::from(format!("{origin}/greeting")))
            .unwrap();
        assert_eq!(reader.next(&mut served.pool), data("fetched hello"));
        drop(sender);
        assert_eq!(reader.next(&mut served.pool), data("end"));
        assert!(matches!(reader.next(&mut served.pool), Frame::Trailers(_)));
        assert_eq!(reader.next(&mut served.pool), Frame::End);
        // The root ends with the call: the host settles without any observer.
        served.pool.run_until(served.host.settled());
        let (_, exit) = status(&mut served);
        // open, the fetched body, end.
        assert!(exit.starts_with("complete events=3 transmit=Ok"), "{exit}");
        close(served, &engine);
    }
}

/// A client disconnect ends a guest that is writing at its next write, and a guest idle in a
/// request read through the closed request body; a watching guest learns it through its
/// transmission result. Each time the call settles and the same Store serves the next request.
#[test]
#[ignore = "requires TEMBRA_WASM_HTTP_HANDLER"]
fn disconnects_end_writing_idle_and_watching_handlers() {
    let Some(component) = fixture("TEMBRA_WASM_HTTP_HANDLER") else {
        return;
    };
    for engine in engines(Limits::default()) {
        let mut served = install(
            &engine,
            &artifact(&component, "handler", 1),
            Install::default(),
        );
        // Writing: the producer writes until its reader is gone.
        let (_, mut reader, settled, _sender) = start(&mut served, "/stream");
        assert_eq!(reader.next(&mut served.pool), data("0"));
        assert_eq!(reader.next(&mut served.pool), data("1"));
        reader.disconnect();
        settle(&mut served.pool, settled).unwrap();
        let (_, exit) = status(&mut served);
        assert!(exit.starts_with("write-dropped"), "{exit}");

        // Idle in a request read: the request body stays open on the application side, but
        // the call closes it for the guest.
        let (_, mut reader, settled, sender) = start(&mut served, "/sse");
        assert_eq!(reader.next(&mut served.pool), data("open"));
        reader.disconnect();
        settle(&mut served.pool, settled).unwrap();
        let (_, exit) = status(&mut served);
        // The final write can reach the host buffer before the response drop reaches the
        // guest. Either that write observes the drop, or transmission reports termination.
        assert!(
            exit == "request-ended-then-write-dropped events=1"
                || exit == "complete events=2 transmit=Err(ErrorCode::ConnectionTerminated)",
            "{exit}"
        );
        drop(sender);

        // Watching the transmission result.
        let (_, mut reader, settled, sender) = start(&mut served, "/watch");
        assert_eq!(reader.next(&mut served.pool), data("open"));
        reader.disconnect();
        settle(&mut served.pool, settled).unwrap();
        let (_, exit) = status(&mut served);
        assert!(exit.starts_with("transmit-resolved Err"), "{exit}");
        assert!(exit.contains("ConnectionTerminated"), "{exit}");
        drop(sender);
        close(served, &engine);
    }
}

/// Cancelling the request's root ends the stream for both the guest and the application; the
/// Store stays usable.
#[test]
#[ignore = "requires TEMBRA_WASM_HTTP_HANDLER"]
fn cancellation_ends_a_streaming_call() {
    let Some(component) = fixture("TEMBRA_WASM_HTTP_HANDLER") else {
        return;
    };
    for engine in engines(Limits::default()) {
        let mut served = install(
            &engine,
            &artifact(&component, "handler", 1),
            Install::default(),
        );
        let (_, mut reader, settled, sender) = start(&mut served, "/sse");
        assert_eq!(reader.next(&mut served.pool), data("open"));
        settled.cancel();
        assert!(matches!(reader.next(&mut served.pool), Frame::Error(_)));
        // Completion reports the cancellation even though the guest ended cooperatively.
        let error = settle(&mut served.pool, settled).unwrap_err();
        assert_eq!(error.code, tembra::core::Code::Cancelled, "{error}");
        drop(sender);
        let (requests, exit) = status(&mut served);
        assert_eq!(requests, 2);
        assert!(exit.starts_with("request-ended"), "{exit}");
        close(served, &engine);
    }
}

/// A handler error and a trap are reported after settlement; the error leaves the Store usable,
/// the trap quarantines it.
#[test]
#[ignore = "requires TEMBRA_WASM_HTTP_HANDLER"]
fn handler_errors_and_traps_are_reported_after_settlement() {
    let Some(component) = fixture("TEMBRA_WASM_HTTP_HANDLER") else {
        return;
    };
    for engine in engines(Limits::default()) {
        let mut served = install(
            &engine,
            &artifact(&component, "handler", 1),
            Install::default(),
        );
        let (incoming, _sender) = request("/error");
        let error = served
            .pool
            .run_until(serve(
                &served.host,
                "alice",
                served.reference.clone(),
                incoming,
            ))
            .err()
            .unwrap();
        assert_eq!(error.reason, "wasi_http_handler", "{error}");
        assert!(error.message.contains("refused"), "{error}");
        let (requests, _) = status(&mut served);
        assert_eq!(requests, 2);
        let (incoming, _sender) = request("/trap");
        let error = served
            .pool
            .run_until(serve(
                &served.host,
                "alice",
                served.reference.clone(),
                incoming,
            ))
            .err()
            .unwrap();
        assert!(error.message.contains("trap"), "{error}");
        let (incoming, _sender) = request("/status");
        let error = served
            .pool
            .run_until(serve(
                &served.host,
                "alice",
                served.reference.clone(),
                incoming,
            ))
            .err()
            .unwrap();
        assert_eq!(error.reason, "wasm_executor_quarantined", "{error}");
        close(served, &engine);
    }
}

/// The engine's call timeout bounds the whole streamed response; the timeout is terminal.
#[test]
#[ignore = "requires TEMBRA_WASM_HTTP_HANDLER"]
fn the_call_timeout_bounds_a_streamed_response() {
    let Some(component) = fixture("TEMBRA_WASM_HTTP_HANDLER") else {
        return;
    };
    let limits = Limits {
        timeout_ms: 1500,
        ..Limits::default()
    };
    for engine in engines(limits) {
        let mut served = install(
            &engine,
            &artifact(&component, "handler", 1),
            Install::default(),
        );
        let (_, mut reader, settled, sender) = start(&mut served, "/sse");
        assert_eq!(reader.next(&mut served.pool), data("open"));
        let error = settle(&mut served.pool, settled).unwrap_err();
        assert!(error.message.contains("timed out"), "{error}");
        assert!(matches!(reader.next(&mut served.pool), Frame::Error(_)));
        drop(sender);
        let (incoming, _sender) = request("/status");
        let error = served
            .pool
            .run_until(serve(
                &served.host,
                "alice",
                served.reference.clone(),
                incoming,
            ))
            .err()
            .unwrap();
        assert_eq!(error.reason, "wasm_executor_quarantined", "{error}");
        close(served, &engine);
    }
}

/// Reload routes new requests to the new revision while the old one finishes its stream; the
/// old Store retires only after that call settled.
#[test]
#[ignore = "requires TEMBRA_WASM_HTTP_HANDLER"]
fn reload_drains_a_streaming_call_and_routes_new_requests() {
    let Some(component) = fixture("TEMBRA_WASM_HTTP_HANDLER") else {
        return;
    };
    for engine in engines(Limits::default()) {
        let artifact = artifact(&component, "handler", 1);
        let mut served = install(&engine, &artifact, Install::default());
        let (_, mut reader, settled, sender) = start(&mut served, "/sse");
        assert_eq!(reader.next(&mut served.pool), data("open"));
        let replacement = engine.load(&artifact, Install::default()).unwrap();
        served
            .pool
            .run_until(served.host.reload(vec![replacement], 20_000))
            .unwrap();
        assert_eq!(engine.instances(), 2);
        // A fresh Store serves the new request although the old one is still streaming.
        let (requests, _) = status(&mut served);
        assert_eq!(requests, 1);
        // The old call keeps streaming on its own revision.
        sender.unbounded_send(Bytes::from("late")).unwrap();
        assert_eq!(reader.next(&mut served.pool), data("late"));
        drop(sender);
        assert_eq!(reader.next(&mut served.pool), data("end"));
        assert!(matches!(reader.next(&mut served.pool), Frame::Trailers(_)));
        assert_eq!(reader.next(&mut served.pool), Frame::End);
        settle(&mut served.pool, settled).unwrap();
        served.pool.run_until(served.host.settled());
        assert_eq!(engine.instances(), 1);
        close(served, &engine);
    }
}

/// The handler is not a portable service: every portable call is refused before the guest is
/// entered, and a source artifact or foreign port cannot install it.
#[test]
#[ignore = "requires TEMBRA_WASM_HTTP_HANDLER"]
fn the_handler_is_host_local() {
    let Some(component) = fixture("TEMBRA_WASM_HTTP_HANDLER") else {
        return;
    };
    let engine = Engine::new(Limits::default()).unwrap();
    let mut served = install(
        &engine,
        &artifact(&component, "handler", 1),
        Install::default(),
    );
    let reference = served.reference.clone();
    let error = served
        .pool
        .run_until(served.host.root("alice", move |context| {
            async move {
                reference
                    .call_portable::<_, serde_json::Value>(
                        &context,
                        "handle",
                        serde_json::json!([null]),
                    )
                    .await
            }
            .boxed_local()
        }))
        .err()
        .unwrap();
    assert_eq!(error.reason, "local_value", "{error}");
    let (requests, _) = status(&mut served);
    assert_eq!(
        requests, 1,
        "the guest was not entered by the portable call"
    );
    // A binary that does not export the handler cannot be packaged for a handler world.
    let client =
        std::env::var_os("TEMBRA_WASM_HTTP_CLIENT").map(|path| std::fs::read(path).unwrap());
    if let Some(client) = client {
        assert!(
            Contract::from_dir(
                concat!(env!("CARGO_MANIFEST_DIR"), "/tests/wit/handler"),
                Some("handler"),
            )
            .unwrap()
            .component_artifact(&client, ORB, 1, &Default::default())
            .is_err()
        );
    }
    close(served, &engine);
}

/// Dropping `serve` before the head cancels the request: a guest waiting for request data to
/// compute its head sees the request end, its late response is refused, the revision is free at
/// once and serves the next request without quarantine.
#[test]
#[ignore = "requires TEMBRA_WASM_HTTP_HANDLER"]
fn dropping_serve_before_the_head_cancels_the_request() {
    let Some(component) = fixture("TEMBRA_WASM_HTTP_HANDLER") else {
        return;
    };
    for engine in engines(Limits::default()) {
        let mut served = install(
            &engine,
            &artifact(&component, "handler", 1),
            Install::default(),
        );
        // The application keeps its request body open; the call closes it for the guest.
        let (incoming, _held) = request("/slow");
        let mut serving = Box::pin(serve(
            &served.host,
            "alice",
            served.reference.clone(),
            incoming,
        ));
        // Let the guest start and wait for its first request chunk.
        served.pool.run_until(async {
            for _ in 0..50 {
                assert!(futures::poll!(serving.as_mut()).is_pending());
                futures_timer::Delay::new(Duration::from_millis(2)).await;
            }
        });
        drop(serving);
        served.pool.run_until(served.host.settled());
        let (requests, exit) = status(&mut served);
        assert_eq!(requests, 2);
        assert_eq!(exit, "slow-request-ended");
        // A slow head that does arrive still streams normally.
        let (incoming, sender) = request("/slow");
        sender.unbounded_send(Bytes::from("first")).unwrap();
        let (response, settled) = served
            .pool
            .run_until(serve(
                &served.host,
                "alice",
                served.reference.clone(),
                incoming,
            ))
            .unwrap();
        let mut reader = Reader::new(response.into_body());
        assert_eq!(reader.next(&mut served.pool), data("open"));
        drop(sender);
        assert_eq!(reader.next(&mut served.pool), data("end"));
        assert!(matches!(reader.next(&mut served.pool), Frame::Trailers(_)));
        assert_eq!(reader.next(&mut served.pool), Frame::End);
        settle(&mut served.pool, settled).unwrap();
        close(served, &engine);
    }
}

/// An `error-code` returned while a task of the call keeps running is reported only once that
/// task ended, so no work outlives the returned error (S5). With no response to deliver, the call
/// closes the request body, so a task reading it ends at once.
#[test]
#[ignore = "requires TEMBRA_WASM_HTTP_HANDLER"]
fn an_error_with_a_running_task_is_reported_after_it_ends() {
    let Some(component) = fixture("TEMBRA_WASM_HTTP_HANDLER") else {
        return;
    };
    for engine in engines(Limits::default()) {
        let mut served = install(
            &engine,
            &artifact(&component, "handler", 1),
            Install::default(),
        );
        let (incoming, _held) = request("/late-error");
        let error = served
            .pool
            .run_until(serve(
                &served.host,
                "alice",
                served.reference.clone(),
                incoming,
            ))
            .err()
            .unwrap();
        assert_eq!(error.reason, "wasi_http_handler", "{error}");
        assert!(error.message.contains("late"), "{error}");
        // The reading task had ended (closed request) before the error was returned.
        let (_, exit) = status(&mut served);
        assert_eq!(exit, "late-error read 0");
        close(served, &engine);
    }
}

/// A trap after the head ends the body with an error and quarantines the revision.
#[test]
#[ignore = "requires TEMBRA_WASM_HTTP_HANDLER"]
fn a_trap_after_the_head_fails_the_body_and_quarantines() {
    let Some(component) = fixture("TEMBRA_WASM_HTTP_HANDLER") else {
        return;
    };
    for engine in engines(Limits::default()) {
        let mut served = install(
            &engine,
            &artifact(&component, "handler", 1),
            Install::default(),
        );
        let (_, mut reader, settled, sender) = start(&mut served, "/trap-after-head");
        assert_eq!(reader.next(&mut served.pool), data("open"));
        sender.unbounded_send(Bytes::from("go")).unwrap();
        // Never a clean end: the guest had resolved its trailers, but the response is truncated.
        let frame = reader.next(&mut served.pool);
        assert!(matches!(frame, Frame::Error(_)), "{frame:?}");
        let error = settle(&mut served.pool, settled).unwrap_err();
        assert!(error.message.contains("trap"), "{error}");
        let (incoming, _sender) = request("/status");
        let error = served
            .pool
            .run_until(serve(
                &served.host,
                "alice",
                served.reference.clone(),
                incoming,
            ))
            .err()
            .unwrap();
        assert_eq!(error.reason, "wasm_executor_quarantined", "{error}");
        close(served, &engine);
    }
}

/// A `serve` dropped right after its first poll, before its root task ever ran, and one dropped
/// while queued behind a streaming request both cancel: neither reaches the guest, and the
/// revision serves the next request without quarantine.
#[test]
#[ignore = "requires TEMBRA_WASM_HTTP_HANDLER"]
fn dropping_serve_before_its_root_or_while_queued_never_reaches_the_guest() {
    let Some(component) = fixture("TEMBRA_WASM_HTTP_HANDLER") else {
        return;
    };
    for engine in engines(Limits::default()) {
        let mut served = install(
            &engine,
            &artifact(&component, "handler", 1),
            Install::default(),
        );
        // Before the root task ran: the first poll only spawns it.
        let (incoming, _held) = request("/sse");
        let mut serving = Box::pin(serve(
            &served.host,
            "alice",
            served.reference.clone(),
            incoming,
        ));
        let waker = futures::task::noop_waker();
        let mut context = std::task::Context::from_waker(&waker);
        assert!(serving.as_mut().poll(&mut context).is_pending());
        drop(serving);
        served.pool.run_until(served.host.settled());
        let (requests, _) = status(&mut served);
        assert_eq!(requests, 1, "the dropped request reached the guest");

        // Queued behind a streaming request on the revision's one callback slot.
        let (_, mut reader, settled, sender) = start(&mut served, "/sse");
        assert_eq!(reader.next(&mut served.pool), data("open"));
        let (incoming, _queued) = request("/status");
        let mut queued = Box::pin(serve(
            &served.host,
            "alice",
            served.reference.clone(),
            incoming,
        ));
        served.pool.run_until(async {
            for _ in 0..20 {
                assert!(futures::poll!(queued.as_mut()).is_pending());
                futures_timer::Delay::new(Duration::from_millis(2)).await;
            }
        });
        drop(queued);
        // The streaming request is unaffected.
        sender.unbounded_send(Bytes::from("still")).unwrap();
        assert_eq!(reader.next(&mut served.pool), data("still"));
        drop(sender);
        assert_eq!(reader.next(&mut served.pool), data("end"));
        assert!(matches!(reader.next(&mut served.pool), Frame::Trailers(_)));
        assert_eq!(reader.next(&mut served.pool), Frame::End);
        settle(&mut served.pool, settled).unwrap();
        served.pool.run_until(served.host.settled());
        // Requests the guest saw: status, the stream, this status; never the queued one.
        let (requests, _) = status(&mut served);
        assert_eq!(requests, 3);
        close(served, &engine);
    }
}

/// Closing the host with a stream in flight cancels it: the body ends with an error, completion
/// reports the cancellation, and the close waits for physical settlement.
#[test]
#[ignore = "requires TEMBRA_WASM_HTTP_HANDLER"]
fn host_close_ends_an_in_flight_stream() {
    let Some(component) = fixture("TEMBRA_WASM_HTTP_HANDLER") else {
        return;
    };
    for engine in engines(Limits::default()) {
        let mut served = install(
            &engine,
            &artifact(&component, "handler", 1),
            Install::default(),
        );
        let (_, mut reader, settled, _held) = start(&mut served, "/sse");
        assert_eq!(reader.next(&mut served.pool), data("open"));
        served.reference.release().unwrap();
        let host = served.host.clone();
        let closing = served.pool.spawner();
        let (closed, done) = futures::channel::oneshot::channel();
        futures::task::LocalSpawnExt::spawn_local(&closing, async move {
            let _ = closed.send(host.close().await);
        })
        .unwrap();
        assert!(matches!(reader.next(&mut served.pool), Frame::Error(_)));
        let error = settle(&mut served.pool, settled).unwrap_err();
        assert_eq!(error.code, tembra::core::Code::Cancelled, "{error}");
        served.pool.run_until(done).unwrap().unwrap();
        assert_eq!(engine.instances(), 0);
    }
}

/// A cancellation before the head (here `Host::close` while the guest waits for request data to
/// compute it) is reported as `Cancelled`, whatever the guest answered while cooperating.
#[test]
#[ignore = "requires TEMBRA_WASM_HTTP_HANDLER"]
fn a_cancellation_before_the_head_reports_cancelled() {
    let Some(component) = fixture("TEMBRA_WASM_HTTP_HANDLER") else {
        return;
    };
    for engine in engines(Limits::default()) {
        let mut served = install(
            &engine,
            &artifact(&component, "handler", 1),
            Install::default(),
        );
        let (incoming, _held) = request("/slow");
        let mut serving = Box::pin(serve(
            &served.host,
            "alice",
            served.reference.clone(),
            incoming,
        ));
        served.pool.run_until(async {
            for _ in 0..20 {
                assert!(futures::poll!(serving.as_mut()).is_pending());
                futures_timer::Delay::new(Duration::from_millis(2)).await;
            }
        });
        served.reference.release().unwrap();
        let host = served.host.clone();
        let (closed, done) = futures::channel::oneshot::channel();
        futures::task::LocalSpawnExt::spawn_local(&served.pool.spawner(), async move {
            let _ = closed.send(host.close().await);
        })
        .unwrap();
        let error = served.pool.run_until(serving).err().unwrap();
        assert_eq!(error.code, tembra::core::Code::Cancelled, "{error}");
        served.pool.run_until(done).unwrap().unwrap();
        assert_eq!(engine.instances(), 0);
    }
}
