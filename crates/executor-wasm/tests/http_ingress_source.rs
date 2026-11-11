//! A source (TypeScript) Orb serves incoming HTTP through the native interpreter, on Cranelift
//! and Pulley, with the same facade, lifetime and settlement as compiled handlers.
//!
//! The Orb is the SDK's `wasm-ingress` fixture, packaged by `scripts/test-wasm-fixtures.ts`.
//! It streams SSE events, calls a `ledger` dependency after its head, echoes raw
//! bytes, duplicate fields and trailers, and reports how each transmission ended to the ledger.
use bytes::Bytes;
use futures::{
    FutureExt, StreamExt,
    channel::mpsc::{UnboundedSender, unbounded},
    executor::LocalPool,
};
use http_body_util::{BodyExt, StreamBody};
use serde_json::{Value, json};
use std::{convert::Infallible, rc::Rc, sync::mpsc, thread, time::Duration};
use tembra::{Host, Reference};
use tembra_executor_wasm::{
    Engine, Install, Limits,
    ingress::{ResponseBody, Settled, handler_service, serve},
};

/// The packaged fixture artifact `<TEMBRA_WASM_FIXTURES>/<name>.wasm`.
fn fixture(name: &str) -> Vec<u8> {
    let directory = std::env::var_os("TEMBRA_WASM_FIXTURES").expect("TEMBRA_WASM_FIXTURES");
    std::fs::read(std::path::Path::new(&directory).join(format!("{name}.wasm"))).unwrap()
}

fn engines(limits: Limits) -> Option<[Engine; 2]> {
    std::env::var_os("TEMBRA_WASM_FIXTURES")?;
    Some([
        Engine::new(limits).unwrap(),
        Engine::interpreted(limits).unwrap(),
    ])
}

struct Served {
    pool: LocalPool,
    host: Host,
    handler: Rc<Reference>,
    ledger: Rc<Reference>,
}

fn install(engine: &Engine) -> Served {
    let pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let ledger = engine.load(&fixture("ledger"), Install::default()).unwrap();
    let ingress = engine
        .load(
            &fixture("ingress"),
            Install::default().requires("example:ingress/ledger@1.0.0", "ledger", 1),
        )
        .unwrap();
    host.install(vec![ledger, ingress]).unwrap();
    let mut served = Served {
        handler: Rc::new(
            host.get(&handler_service("ingress"), vec!["main".into()], "alice")
                .unwrap(),
        ),
        ledger: Rc::new(
            host.get(
                "ledger::example:ingress/ledger@1.0.0",
                vec!["main".into()],
                "alice",
            )
            .unwrap(),
        ),
        pool,
        host,
    };
    served.pool.run_until(served.handler.ready()).unwrap();
    served.pool.run_until(served.ledger.ready()).unwrap();
    served
}

type Body = StreamBody<
    futures::stream::Map<
        futures::channel::mpsc::UnboundedReceiver<http_body::Frame<Bytes>>,
        fn(http_body::Frame<Bytes>) -> Result<http_body::Frame<Bytes>, Infallible>,
    >,
>;

/// A request whose frames the test sends; dropping the sender ends it.
fn request(
    path: &str,
    fields: &[(&str, &[u8])],
) -> (
    http::Request<Body>,
    UnboundedSender<http_body::Frame<Bytes>>,
) {
    let (sender, receiver) = unbounded();
    let frame: fn(http_body::Frame<Bytes>) -> Result<http_body::Frame<Bytes>, Infallible> = Ok;
    let mut request = http::Request::post(format!("http://ingress.test{path}"))
        .body(StreamBody::new(receiver.map(frame)))
        .unwrap();
    for (name, value) in fields {
        request.headers_mut().append(
            http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            http::HeaderValue::from_bytes(value).unwrap(),
        );
    }
    (request, sender)
}

fn chunk(text: &str) -> http_body::Frame<Bytes> {
    http_body::Frame::data(Bytes::from(text.to_owned()))
}

#[derive(Debug, PartialEq)]
enum Frame {
    Data(Vec<u8>),
    Trailers(Vec<(String, Vec<u8>)>),
    End,
    Error(String),
}

fn data(text: &str) -> Frame {
    Frame::Data(text.as_bytes().to_vec())
}

/// Reads a response body on its own thread, like a server connection.
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
                        Ok(data) => Frame::Data(data.to_vec()),
                        Err(frame) => Frame::Trailers(
                            frame
                                .into_trailers()
                                .unwrap()
                                .iter()
                                .map(|(name, value)| (name.to_string(), value.as_bytes().to_vec()))
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

    fn next(&mut self, pool: &mut LocalPool) -> Frame {
        self.commands.send(true).unwrap();
        let frame = self.frames.next();
        let deadline = futures_timer::Delay::new(Duration::from_secs(20));
        match pool.run_until(futures::future::select(frame, deadline)) {
            futures::future::Either::Left((Some(frame), _)) => frame,
            _ => panic!("no response frame within the bound"),
        }
    }

    fn disconnect(&self) {
        self.commands.send(false).unwrap();
    }
}

fn start(
    served: &mut Served,
    path: &str,
    fields: &[(&str, &[u8])],
) -> (
    http::response::Parts,
    Reader,
    Settled,
    UnboundedSender<http_body::Frame<Bytes>>,
) {
    let (incoming, sender) = request(path, fields);
    let (response, settled) = served
        .pool
        .run_until(serve(
            &served.host,
            "alice",
            served.handler.clone(),
            incoming,
        ))
        .unwrap();
    let (parts, body) = response.into_parts();
    (parts, Reader::new(body), settled, sender)
}

fn settle(pool: &mut LocalPool, settled: Settled) -> tembra::Result<()> {
    let deadline = futures_timer::Delay::new(Duration::from_secs(20));
    match pool.run_until(futures::future::select(settled, deadline)) {
        futures::future::Either::Left((result, _)) => result,
        futures::future::Either::Right(_) => panic!("the request did not settle"),
    }
}

fn pending(pool: &mut LocalPool, settled: &mut Settled) -> bool {
    pool.run_until_stalled();
    settled.now_or_never().is_none()
}

/// The ledger's entries, read through an ordinary portable call.
fn entries(served: &mut Served) -> Vec<String> {
    let ledger = served.ledger.clone();
    let value: Value = served
        .pool
        .run_until(served.host.root("alice", move |context| {
            async move {
                ledger
                    .call_portable::<_, serde_json::Value>(&context, "entries", json!([]))
                    .await
            }
            .boxed_local()
        }))
        .unwrap();
    serde_json::from_value(value).unwrap()
}

/// Wait until the ledger records `entry` (transmission results are noted asynchronously).
fn noted(served: &mut Served, entry: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !entries(served).iter().any(|noted| noted == entry) {
        assert!(
            std::time::Instant::now() < deadline,
            "{entry} was not noted"
        );
        served
            .pool
            .run_until(futures_timer::Delay::new(Duration::from_millis(5)));
    }
}

/// The served count of this revision's instance; proves the Store still serves requests.
fn revision(served: &mut Served) -> u32 {
    let (parts, mut reader, settled, _sender) = start(served, "/revision", &[]);
    assert_eq!(parts.status, 200);
    let Frame::Data(count) = reader.next(&mut served.pool) else {
        panic!("expected the count")
    };
    assert_eq!(reader.next(&mut served.pool), Frame::End);
    settle(&mut served.pool, settled).unwrap();
    String::from_utf8(count).unwrap().parse().unwrap()
}

fn close(mut served: Served, engine: &Engine) {
    served.handler.release().unwrap();
    served.ledger.release().unwrap();
    served.pool.run_until(served.host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
}

/// Head and incremental SSE before EOF; request chunks stream in; after the head (with the
/// completion observer dropped) the guest calls its dependency under the request's root; the
/// trailers follow; the transmission ends `ok`.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn source_handlers_stream_before_eof_and_keep_authority_after_the_head() {
    let Some(engines) = engines(Limits::default()) else {
        return;
    };
    for engine in engines {
        let mut served = install(&engine);
        let (parts, mut reader, mut settled, sender) = start(&mut served, "/sse", &[]);
        assert_eq!(parts.status, 200);
        assert_eq!(parts.headers["content-type"], "text/event-stream");
        assert!(pending(&mut served.pool, &mut settled));
        assert_eq!(reader.next(&mut served.pool), data("data: 1\n\n"));
        assert!(pending(&mut served.pool, &mut settled));
        sender.unbounded_send(chunk("a")).unwrap();
        assert_eq!(reader.next(&mut served.pool), data("data: tick-1:1\n\n"));
        sender.unbounded_send(chunk("bb")).unwrap();
        assert_eq!(reader.next(&mut served.pool), data("data: tick-2:2\n\n"));
        // Nothing waits for completion any more; the root must keep the guest's authority.
        drop(settled);
        drop(sender);
        let Frame::Data(noted_event) = reader.next(&mut served.pool) else {
            panic!("expected the dependency event")
        };
        assert!(
            String::from_utf8(noted_event)
                .unwrap()
                .starts_with("data: noted-")
        );
        assert_eq!(
            reader.next(&mut served.pool),
            Frame::Trailers(vec![("x-ticks".into(), b"2".to_vec())])
        );
        assert_eq!(reader.next(&mut served.pool), Frame::End);
        served.pool.run_until(served.host.settled());
        noted(&mut served, "sse-after-head:2");
        noted(&mut served, "sse-transmitted:ok");
        assert_eq!(revision(&mut served), 2);
        close(served, &engine);
    }
}

/// Raw byte values, duplicate fields, a status and trailers survive both directions.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn source_handlers_echo_raw_fields_bodies_and_trailers() {
    let Some(engines) = engines(Limits::default()) else {
        return;
    };
    for engine in engines {
        let mut served = install(&engine);
        let (parts, mut reader, settled, sender) = start(
            &mut served,
            "/echo",
            &[("x-raw", &[0x80, 0xff]), ("x-raw", b"two")],
        );
        assert_eq!(parts.status, 201);
        assert_eq!(parts.headers["x-method"], "post");
        assert_eq!(parts.headers["x-seen"], "2");
        let raw: Vec<&[u8]> = parts
            .headers
            .get_all("x-raw")
            .iter()
            .map(|value| value.as_bytes())
            .collect();
        assert_eq!(raw, [&[0x80, 0xff][..], b"!"]);
        sender
            .unbounded_send(http_body::Frame::data(Bytes::from_static(&[0, 0xfe, 1])))
            .unwrap();
        assert_eq!(reader.next(&mut served.pool), Frame::Data(vec![0, 0xfe, 1]));
        let mut trailers = http::HeaderMap::new();
        trailers.append("x-request-trailer", http::HeaderValue::from_static("t"));
        sender
            .unbounded_send(http_body::Frame::trailers(trailers))
            .unwrap();
        drop(sender);
        assert_eq!(
            reader.next(&mut served.pool),
            Frame::Trailers(vec![("x-request-trailer".into(), b"t".to_vec())])
        );
        assert_eq!(reader.next(&mut served.pool), Frame::End);
        settle(&mut served.pool, settled).unwrap();
        noted(&mut served, "echo-transmitted:ok");
        close(served, &engine);
    }
}

/// A disconnect while the guest is idle in a request read ends that read; one while it is
/// writing fails its next write; both report `connection-terminated` to the guest, and the
/// Store keeps serving.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn source_disconnects_end_idle_reads_and_writes() {
    let Some(engines) = engines(Limits::default()) else {
        return;
    };
    for engine in engines {
        let mut served = install(&engine);
        let (_, mut reader, settled, _held) = start(&mut served, "/idle", &[]);
        assert_eq!(reader.next(&mut served.pool), data("data: 1\n\n"));
        reader.disconnect();
        settle(&mut served.pool, settled).unwrap();
        noted(&mut served, "idle-read-ended");
        noted(&mut served, "idle-transmitted:err");

        let (_, mut reader, settled, _held) = start(&mut served, "/write", &[]);
        assert_eq!(reader.next(&mut served.pool), data("data: 0\n\n"));
        reader.disconnect();
        settle(&mut served.pool, settled).unwrap();
        noted(&mut served, "write-transmitted:err");
        assert_eq!(revision(&mut served), 3);
        close(served, &engine);
    }
}

/// A standard WASI error stays recoverable; cancellation ends a stream for both sides and
/// reports `Cancelled`; failures without a WIT error lane quarantine the revision.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn source_errors_cancellation_and_traps() {
    let Some(engines) = engines(Limits::default()) else {
        return;
    };
    for engine in engines {
        // A standard WASI result error is the handler's typed error and is recoverable: the
        // revision keeps serving.
        let mut served = install(&engine);
        let error = once(&mut served, "/error").unwrap_err();
        assert_eq!(error.reason, "wasi_http_handler", "{error}");
        assert!(error.message.contains("HttpRequestDenied"), "{error}");
        assert_eq!(once(&mut served, "/revision").unwrap(), 200);
        close(served, &engine);

        let mut served = install(&engine);
        let (_, mut reader, settled, _held) = start(&mut served, "/sse", &[]);
        assert_eq!(reader.next(&mut served.pool), data("data: 1\n\n"));
        settled.cancel();
        assert!(matches!(reader.next(&mut served.pool), Frame::Error(_)));
        let error = settle(&mut served.pool, settled).unwrap_err();
        assert_eq!(error.code, tembra::core::Code::Cancelled, "{error}");
        // The cancelled handler's delivery observer still calls its dependency; the refused
        // dependency import has no WIT error lane, so the revision fails closed.
        let after = once(&mut served, "/revision").unwrap_err();
        assert_eq!(after.reason, "wasm_executor_quarantined", "{after}");
        assert!(after.message.contains("scope is cancelled"), "{after}");
        retire(served, &engine);

        // An exception that is not the handler's WIT error traps and quarantines the revision.
        let mut served = install(&engine);
        let error = once(&mut served, "/trap").unwrap_err();
        assert!(error.message.contains("component trap"), "{error}");
        let after = once(&mut served, "/revision").unwrap_err();
        assert_eq!(after.reason, "wasm_executor_quarantined", "{after}");
        retire(served, &engine);
    }
}

/// Serve one request to completion; the outcome is the handler's error or its settled status.
fn once(served: &mut Served, path: &str) -> tembra::Result<u16> {
    let (incoming, _sender) = request(path, &[]);
    let (response, settled) = served.pool.run_until(serve(
        &served.host,
        "alice",
        served.handler.clone(),
        incoming,
    ))?;
    let status = response.status().as_u16();
    let mut reader = Reader::new(response.into_body());
    while matches!(reader.next(&mut served.pool), Frame::Data(_)) {}
    settle(&mut served.pool, settled).map(|()| status)
}

/// Release a quarantined revision: closing may report its cause, and nothing stays live.
fn retire(mut served: Served, engine: &Engine) {
    let _ = served.handler.release();
    let _ = served.ledger.release();
    let _ = served.pool.run_until(served.host.close());
    assert_eq!(engine.instances(), 0);
}

/// The call timeout bounds the whole streamed response and is terminal.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn source_timeout_bounds_a_streamed_response() {
    let Some(engines) = engines(Limits {
        timeout_ms: 1500,
        ..Limits::default()
    }) else {
        return;
    };
    for engine in engines {
        let mut served = install(&engine);
        let (_, mut reader, settled, _held) = start(&mut served, "/slow", &[]);
        assert_eq!(reader.next(&mut served.pool), data("data: 1\n\n"));
        let error = settle(&mut served.pool, settled).unwrap_err();
        assert!(!error.message.is_empty(), "{error}");
        assert!(matches!(reader.next(&mut served.pool), Frame::Error(_)));
        let (incoming, _sender) = request("/revision", &[]);
        let error = served
            .pool
            .run_until(serve(
                &served.host,
                "alice",
                served.handler.clone(),
                incoming,
            ))
            .err()
            .unwrap();
        assert_eq!(error.reason, "wasm_executor_quarantined", "{error}");
        close(served, &engine);
    }
}

/// Reload routes new requests to a fresh revision while the old one finishes its stream.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn source_reload_drains_and_routes() {
    let Some(engines) = engines(Limits::default()) else {
        return;
    };
    for engine in engines {
        let mut served = install(&engine);
        // Prepare the candidate first: compiling it is not part of the stream's call budget.
        let replacement = engine
            .load(
                &fixture("ingress"),
                Install::default().requires("example:ingress/ledger@1.0.0", "ledger", 1),
            )
            .unwrap();
        let (_, mut reader, settled, sender) = start(&mut served, "/sse", &[]);
        assert_eq!(reader.next(&mut served.pool), data("data: 1\n\n"));
        served
            .pool
            .run_until(served.host.reload(vec![replacement], 20_000))
            .unwrap();
        assert_eq!(revision(&mut served), 1);
        sender.unbounded_send(chunk("late")).unwrap();
        assert_eq!(reader.next(&mut served.pool), data("data: tick-1:4\n\n"));
        drop(sender);
        assert!(matches!(reader.next(&mut served.pool), Frame::Data(_)));
        assert!(matches!(reader.next(&mut served.pool), Frame::Trailers(_)));
        assert_eq!(reader.next(&mut served.pool), Frame::End);
        settle(&mut served.pool, settled).unwrap();
        served.pool.run_until(served.host.settled());
        close(served, &engine);
    }
}
