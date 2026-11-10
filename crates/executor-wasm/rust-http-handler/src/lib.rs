//! A compiled Orb serving the standard `wasi:http/handler@0.3.0` export with streamed responses.
//!
//! `handle` returns its response at once; a producer task keeps writing the body inside the same
//! export task. The request path selects the behavior:
//!
//! - `/sse`: write `data: open`, then one event echoing each request body chunk; on request end
//!   write `data: end`, close the body with an `x-events` trailer and await the transmission
//!   result. A dropped response ends the producer at its next write.
//! - `/watch`: write `data: open`, then wait only for the transmission result, never reading the
//!   request body: a disconnect must reach it through that result alone.
//! - `/stream`: write numbered events until the response is dropped.
//! - `/proxy` (`proxy` feature): like `/sse`, but the first request chunk is a URL; the handler
//!   sends a GET to it through `wasi:http/client` and writes the fetched body as an event.
//! - `/error`: return an `error-code`; `/trap`: trap.
//! - `/slow`: read one request chunk before answering, then stream like `/sse` with it echoed:
//!   a request that ends or closes first gets `slow-request-ended`.
//! - `/late-error`: return an `error-code` while a task keeps reading the request to its end.
//! - `/trap-after-head`: stream `data: open`, then trap on the first request chunk.
//! - `/status`: no body; `x-requests` counts requests this Store has served and `x-last-exit`
//!   reports how the previous producer ended.
#![no_std]
extern crate alloc;

use alloc::{
    format,
    string::{String, ToString},
    vec::Vec,
};
use core::{cell::Cell, pin::pin};
use wasi::http::types::{
    DnsErrorPayload, ErrorCode, FieldSizePayload, Fields, Request, Response, Trailers,
};
use wit_bindgen::{FutureReader, FutureWriter, StreamReader, StreamResult, StreamWriter};

#[global_allocator]
static ALLOCATOR: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;

#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    core::arch::wasm32::unreachable()
}

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

/// Per-Store state; the component is single-threaded.
struct Store {
    requests: Cell<u32>,
    last_exit: core::cell::RefCell<String>,
}
// SAFETY: wasm32 components run one thread; the cells are never shared across threads.
unsafe impl Sync for Store {}
static STORE: Store = Store {
    requests: Cell::new(0),
    last_exit: core::cell::RefCell::new(String::new()),
};

fn record(exit: String) {
    *STORE.last_exit.borrow_mut() = exit;
}

/// Write one SSE event; `false` when the response stream was dropped.
async fn emit(tx: &mut StreamWriter<u8>, data: &str) -> bool {
    tx.write_all(format!("data: {data}\n\n").into_bytes())
        .await
        .is_empty()
}

fn fields(entries: &[(&str, String)]) -> Fields {
    let list = entries
        .iter()
        .map(|(name, value)| (name.to_string(), value.clone().into_bytes()))
        .collect::<Vec<_>>();
    Fields::from_list(&list).unwrap()
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Sse,
    Watch,
    #[cfg_attr(not(feature = "proxy"), allow(dead_code))]
    Proxy,
}

struct Producer {
    mode: Mode,
    body: StreamReader<u8>,
    tx: StreamWriter<u8>,
    trailers: FutureWriter<Result<Option<Trailers>, ErrorCode>>,
    transmit: FutureReader<Result<(), ErrorCode>>,
    consumed: FutureWriter<Result<(), ErrorCode>>,
}

/// The body task of `/sse`, `/watch` and `/proxy`; it runs after `handle` returned.
async fn produce(mut p: Producer) {
    let mut events = 0u32;
    if !emit(&mut p.tx, "open").await {
        return record("write-dropped events=0".into());
    }
    events += 1;
    let transmit = pin!(p.transmit.into_future());
    if p.mode == Mode::Watch {
        let result = transmit.await;
        return record(format!("transmit-resolved {result:?} events={events}"));
    }
    let mut fetched = false;
    loop {
        let (status, chunk) = p.body.read(Vec::with_capacity(4096)).await;
        match status {
            StreamResult::Complete(_) if !chunk.is_empty() => {
                let text = String::from_utf8_lossy(&chunk).into_owned();
                let event = if p.mode == Mode::Proxy && !fetched {
                    fetched = true;
                    fetch(&text).await
                } else {
                    text
                };
                if !emit(&mut p.tx, &event).await {
                    return record(format!("write-dropped events={events}"));
                }
                events += 1;
            }
            StreamResult::Complete(_) | StreamResult::Cancelled => {}
            StreamResult::Dropped => break,
        }
    }
    // The request body ended: at its end, or closed by the host.
    if !emit(&mut p.tx, "end").await {
        return record(format!("request-ended-then-write-dropped events={events}"));
    }
    events += 1;
    drop(p.tx);
    let _ = p
        .trailers
        .write(Ok(Some(fields(&[("x-events", events.to_string())]))))
        .await;
    let _ = p.consumed.write(Ok(())).await;
    let result = transmit.await;
    record(format!("complete events={events} transmit={result:?}"));
}

/// The body task of `/stream`: numbered events until the response is dropped.
async fn stream(mut tx: StreamWriter<u8>) {
    let mut events = 0u32;
    loop {
        if !emit(&mut tx, &events.to_string()).await {
            return record(format!("write-dropped events={events}"));
        }
        events += 1;
    }
}

#[cfg(feature = "proxy")]
async fn fetch(url: &str) -> String {
    use wasi::http::{client, types::Scheme};
    let Some(rest) = url.trim().strip_prefix("http://") else {
        return "fetch-error not an http URL".into();
    };
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    let (_, trailers) = wit_future::new::<Result<Option<Trailers>, ErrorCode>>(|| Ok(None));
    let (request, _sent) = Request::new(Fields::new(), None, trailers, None);
    if request.set_scheme(Some(&Scheme::Http)).is_err()
        || request.set_authority(Some(authority)).is_err()
        || request.set_path_with_query(Some(path)).is_err()
    {
        return "fetch-error invalid target".into();
    }
    match client::send(request).await {
        Ok(response) => {
            let (_, consumed) = wit_future::new::<Result<(), ErrorCode>>(|| Ok(()));
            let (body, _trailers) = Response::consume_body(response, consumed);
            let bytes = body.collect().await;
            format!("fetched {}", String::from_utf8_lossy(&bytes))
        }
        Err(error) => format!("fetch-error {error:?}"),
    }
}

#[cfg(not(feature = "proxy"))]
async fn fetch(_: &str) -> String {
    "fetch-error no client".into()
}

/// A response with an SSE body and a trailers future; the producer gets the writing ends.
fn streaming() -> (
    Response,
    StreamWriter<u8>,
    FutureWriter<Result<Option<Trailers>, ErrorCode>>,
    FutureReader<Result<(), ErrorCode>>,
) {
    let (tx, rx) = wit_stream::new::<u8>();
    let (trailers, trailers_rx) =
        wit_future::new::<Result<Option<Trailers>, ErrorCode>>(|| Ok(None));
    let headers = fields(&[
        ("content-type", "text/event-stream".into()),
        ("x-requests", STORE.requests.get().to_string()),
    ]);
    let (response, transmit) = Response::new(headers, Some(rx), trailers_rx);
    (response, tx, trailers, transmit)
}

/// `/slow`: the head depends on the first request chunk.
async fn slow(request: Request) -> Result<Response, ErrorCode> {
    let (consumed, consumed_rx) = wit_future::new::<Result<(), ErrorCode>>(|| Ok(()));
    let (mut body, _trailers) = Request::consume_body(request, consumed_rx);
    let (status, chunk) = body.read(Vec::with_capacity(4096)).await;
    if status == StreamResult::Dropped || chunk.is_empty() {
        record("slow-request-ended".into());
        return Err(ErrorCode::InternalError(Some("no first chunk".into())));
    }
    let (response, tx, trailers, transmit) = streaming();
    record("running".into());
    wit_bindgen::spawn_local(produce(Producer {
        mode: Mode::Sse,
        body,
        tx,
        trailers,
        transmit,
        consumed,
    }));
    Ok(response)
}

struct Handler;

impl exports::wasi::http::handler::Guest for Handler {
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        STORE.requests.set(STORE.requests.get() + 1);
        let path = request.get_path_with_query().unwrap_or_default();
        let mode = match path.as_str() {
            "/sse" => Mode::Sse,
            "/watch" => Mode::Watch,
            #[cfg(feature = "proxy")]
            "/proxy" => Mode::Proxy,
            "/error" => return Err(ErrorCode::InternalError(Some("refused".into()))),
            // Typed refusals with payloads: escaped/unicode text, a record, an exact u64.
            "/refuse/escaped" => {
                return Err(ErrorCode::InternalError(Some(
                    "quote \" slash \\ line\nend é🌍".into(),
                )));
            }
            "/refuse/record" => {
                return Err(ErrorCode::HttpRequestHeaderSize(Some(FieldSizePayload {
                    field_name: Some("x-big".into()),
                    field_size: Some(u32::MAX),
                })));
            }
            "/refuse/u64" => return Err(ErrorCode::HttpResponseBodySize(Some(u64::MAX))),
            "/refuse/dns" => {
                return Err(ErrorCode::DnsError(DnsErrorPayload {
                    rcode: Some("SERVFAIL".into()),
                    info_code: Some(u16::MAX),
                }));
            }
            "/slow" => return slow(request).await,
            "/late-error" => {
                let (_, consumed) = wit_future::new::<Result<(), ErrorCode>>(|| Ok(()));
                let (body, _trailers) = Request::consume_body(request, consumed);
                record("late-error running".into());
                wit_bindgen::spawn_local(async move {
                    let bytes = body.collect().await;
                    record(format!("late-error read {}", bytes.len()));
                });
                return Err(ErrorCode::InternalError(Some("late".into())));
            }
            "/trap-after-head" => {
                let (_, consumed) = wit_future::new::<Result<(), ErrorCode>>(|| Ok(()));
                let (mut body, _trailers) = Request::consume_body(request, consumed);
                let (response, mut tx, _trailers, _transmit) = streaming();
                wit_bindgen::spawn_local(async move {
                    if emit(&mut tx, "open").await {
                        let _ = body.read(Vec::with_capacity(64)).await;
                        core::arch::wasm32::unreachable();
                    }
                });
                return Ok(response);
            }
            "/trap" => core::arch::wasm32::unreachable(),
            "/status" => {
                let headers = fields(&[
                    ("x-requests", STORE.requests.get().to_string()),
                    ("x-last-exit", STORE.last_exit.borrow().clone()),
                ]);
                let (_, trailers) =
                    wit_future::new::<Result<Option<Trailers>, ErrorCode>>(|| Ok(None));
                return Ok(Response::new(headers, None, trailers).0);
            }
            "/stream" => {
                let (response, tx, _trailers, _transmit) = streaming();
                record("running".into());
                wit_bindgen::spawn_local(stream(tx));
                return Ok(response);
            }
            other => {
                return Err(ErrorCode::InternalError(Some(format!(
                    "unknown path {other}"
                ))));
            }
        };
        let (consumed, consumed_rx) = wit_future::new::<Result<(), ErrorCode>>(|| Ok(()));
        let (body, _request_trailers) = Request::consume_body(request, consumed_rx);
        let (response, tx, trailers, transmit) = streaming();
        record("running".into());
        wit_bindgen::spawn_local(produce(Producer {
            mode,
            body,
            tx,
            trailers,
            transmit,
            consumed,
        }));
        Ok(response)
    }
}

export!(Handler);
