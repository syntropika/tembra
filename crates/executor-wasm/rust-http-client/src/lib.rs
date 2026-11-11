//! A compiled Orb using the standard `wasi:http` client import and its resources directly.
//!
//! It is `no_std`, like the probe, so its component imports exactly its WIT world.
#![no_std]
extern crate alloc;

use alloc::{
    format,
    string::{String, ToString},
    vec,
    vec::Vec,
};
use exports::example::fetch::fetcher::{ExchangeReport, RequestSpec, ValueReport};
use wasi::http::{
    client,
    types::{
        ErrorCode, Fields, HeaderError, Method, Request, RequestOptions, RequestOptionsError,
        Response, Scheme, Trailers,
    },
};

#[global_allocator]
static ALLOCATOR: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;

#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    core::arch::wasm32::unreachable()
}

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

/// Send a body-less GET through the host's standard client.
async fn send(scheme: String, authority: String, path: String) -> Result<Response, String> {
    let (_, trailers) = wit_future::new::<Result<Option<Trailers>, ErrorCode>>(|| Ok(None));
    let (request, _sent) = Request::new(Fields::new(), None, trailers, None);
    let scheme = match scheme.as_str() {
        "http" => Scheme::Http,
        "https" => Scheme::Https,
        other => Scheme::Other(other.to_string()),
    };
    request.set_scheme(Some(&scheme)).map_err(|()| "scheme")?;
    request
        .set_authority(Some(&authority))
        .map_err(|()| "authority")?;
    request
        .set_path_with_query(Some(&path))
        .map_err(|()| "path")?;
    client::send(request)
        .await
        .map_err(|error| format!("{error:?}"))
}

struct Fetcher;

/// The response `keep` retains for `close-kept`; the component is single-threaded.
struct Kept(core::cell::RefCell<Option<Response>>);
// SAFETY: wasm32 components run one thread; the cell is never shared across threads.
unsafe impl Sync for Kept {}
static KEPT: Kept = Kept(core::cell::RefCell::new(None));

/// A body stream `read-and-keep` leaves open after its call returns.
struct KeptBody(core::cell::RefCell<Option<wit_bindgen::StreamReader<u8>>>);
// SAFETY: wasm32 components run one thread; the cell is never shared across threads.
unsafe impl Sync for KeptBody {}
static KEPT_BODY: KeptBody = KeptBody(core::cell::RefCell::new(None));

fn check(checks: &mut Vec<String>, name: &str, passed: bool) -> Result<(), String> {
    if !passed {
        return Err(name.to_string());
    }
    checks.push(name.to_string());
    Ok(())
}

fn no_trailers() -> wit_bindgen::FutureReader<Result<Option<Trailers>, ErrorCode>> {
    wit_future::new::<Result<Option<Trailers>, ErrorCode>>(|| Ok(None)).1
}

/// Values deliberately exceed JavaScript's exact Number range; durations must stay u64.
fn value_audit() -> Result<ValueReport, String> {
    let mut checks = Vec::new();
    let fields = Fields::new();
    check(&mut checks, "fields.new", fields.copy_all().is_empty())?;
    let fields = Fields::from_list(&[
        ("X-Raw".into(), vec![0xff, 0x80, 0x41]),
        ("X-Dup".into(), b"one".to_vec()),
        ("x-dup".into(), b"two".to_vec()),
    ])
    .map_err(|error| format!("{error:?}"))?;
    let duplicates = fields.get("X-DUP");
    check(
        &mut checks,
        "fields.get",
        fields.get("x-raw") == vec![vec![0xff, 0x80, 0x41]],
    )?;
    check(
        &mut checks,
        "fields.has",
        fields.has("x-dup") && !fields.has("bad name"),
    )?;
    check(
        &mut checks,
        "fields.duplicates",
        duplicates == vec![b"one".to_vec(), b"two".to_vec()],
    )?;
    check(&mut checks, "fields.copy-all", fields.copy_all().len() == 3)?;
    let copy = fields.clone();
    copy.set("x-dup", &[b"three".to_vec()])
        .map_err(|error| format!("{error:?}"))?;
    check(
        &mut checks,
        "fields.set-and-clone",
        copy.get("x-dup") == vec![b"three".to_vec()] && fields.get("x-dup") == duplicates,
    )?;
    copy.append("x-dup", b"four")
        .map_err(|error| format!("{error:?}"))?;
    check(&mut checks, "fields.append", copy.get("x-dup").len() == 2)?;
    check(
        &mut checks,
        "fields.get-and-delete",
        copy.get_and_delete("x-dup")
            .map_err(|error| format!("{error:?}"))?
            .len()
            == 2
            && !copy.has("x-dup"),
    )?;
    copy.delete("x-raw").map_err(|error| format!("{error:?}"))?;
    check(&mut checks, "fields.delete", copy.copy_all().is_empty())?;
    check(
        &mut checks,
        "fields.invalid",
        matches!(
            copy.append("bad name", b"x"),
            Err(HeaderError::InvalidSyntax)
        ) && matches!(
            copy.append("x-ok", b"x\r\n"),
            Err(HeaderError::InvalidSyntax)
        ),
    )?;
    check(
        &mut checks,
        "fields.forbidden",
        matches!(copy.append("host", b"x"), Err(HeaderError::Forbidden)),
    )?;
    check(
        &mut checks,
        "fields.limit",
        matches!(
            copy.append("x-large", &vec![b'x'; 128 << 10]),
            Err(HeaderError::SizeExceeded)
        ),
    )?;
    copy.set("content-length", &[b"007".to_vec()])
        .map_err(|error| format!("{error:?}"))?;
    check(
        &mut checks,
        "fields.content-length",
        copy.get("content-length") == vec![b"7".to_vec()]
            && copy.append("content-length", b"7").is_err(),
    )?;

    let options = RequestOptions::new();
    check(
        &mut checks,
        "options.defaults",
        options.get_connect_timeout().is_none()
            && options.get_first_byte_timeout().is_none()
            && options.get_between_bytes_timeout().is_none(),
    )?;
    let exact = ((1 << 53) + 1, u64::MAX - 1, u64::MAX);
    options
        .set_connect_timeout(Some(exact.0))
        .map_err(|error| format!("{error:?}"))?;
    options
        .set_first_byte_timeout(Some(exact.1))
        .map_err(|error| format!("{error:?}"))?;
    options
        .set_between_bytes_timeout(Some(exact.2))
        .map_err(|error| format!("{error:?}"))?;
    let cloned = options.clone();
    check(
        &mut checks,
        "options.exact-and-clone",
        cloned.get_connect_timeout() == Some(exact.0)
            && cloned.get_first_byte_timeout() == Some(exact.1)
            && cloned.get_between_bytes_timeout() == Some(exact.2),
    )?;
    cloned
        .set_connect_timeout(None)
        .map_err(|error| format!("{error:?}"))?;
    check(
        &mut checks,
        "options.reset",
        cloned.get_connect_timeout().is_none() && options.get_connect_timeout() == Some(exact.0),
    )?;
    let (request, _sent) = Request::new(fields, None, no_trailers(), Some(options));
    check(
        &mut checks,
        "request.defaults",
        matches!(request.get_method(), Method::Get)
            && request.get_scheme().is_none()
            && request.get_authority().is_none()
            && request.get_path_with_query().is_none(),
    )?;
    request
        .set_method(&Method::Other("PATCH".into()))
        .map_err(|()| "method")?;
    check(
        &mut checks,
        "request.method",
        matches!(request.get_method(), Method::Patch)
            && request
                .set_method(&Method::Other("bad method".into()))
                .is_err(),
    )?;
    request
        .set_scheme(Some(&Scheme::Https))
        .map_err(|()| "scheme")?;
    check(
        &mut checks,
        "request.scheme",
        matches!(request.get_scheme(), Some(Scheme::Https))
            && request
                .set_scheme(Some(&Scheme::Other("bad scheme".into())))
                .is_err(),
    )?;
    request
        .set_authority(Some("example.test:8443"))
        .map_err(|()| "authority")?;
    check(
        &mut checks,
        "request.authority",
        request.get_authority().as_deref() == Some("example.test:8443")
            && request.set_authority(Some("user@example.test")).is_err(),
    )?;
    request
        .set_path_with_query(Some("/raw/../path?q=a#fragment"))
        .map_err(|()| "path")?;
    check(
        &mut checks,
        "request.path",
        request.get_path_with_query().as_deref() == Some("/raw/../path?q=a")
            && request.set_path_with_query(Some("invalid")).is_err(),
    )?;
    let headers = request.get_headers();
    check(
        &mut checks,
        "request.immutable-headers",
        matches!(headers.set("x-dup", &[]), Err(HeaderError::Immutable)),
    )?;
    drop(headers);
    let options = request.get_options().ok_or("missing options")?;
    check(
        &mut checks,
        "request.immutable-options",
        matches!(
            options.set_connect_timeout(None),
            Err(RequestOptionsError::Immutable)
        ) && matches!(
            options.set_first_byte_timeout(None),
            Err(RequestOptionsError::Immutable)
        ) && matches!(
            options.set_between_bytes_timeout(None),
            Err(RequestOptionsError::Immutable)
        ),
    )?;
    drop(options);
    let (body, trailers) = Request::consume_body(
        request,
        wit_future::new::<Result<(), ErrorCode>>(|| Ok(())).1,
    );
    drop(body);
    drop(trailers);
    checks.push("request.consume-body".into());

    let (response, _sent) = Response::new(Fields::new(), None, no_trailers());
    check(
        &mut checks,
        "response.default-status",
        response.get_status_code() == 200,
    )?;
    response
        .set_status_code(201)
        .map_err(|()| "response status")?;
    check(
        &mut checks,
        "response.status",
        response.get_status_code() == 201
            && response.set_status_code(99).is_err()
            && response.set_status_code(600).is_err(),
    )?;
    let headers = response.get_headers();
    check(
        &mut checks,
        "response.immutable-headers",
        matches!(headers.append("x-ok", b"x"), Err(HeaderError::Immutable)),
    )?;
    drop(headers);
    drop(response);
    Ok(ValueReport {
        checks,
        duplicate_values: duplicates,
        exact_timeouts: exact,
    })
}

impl exports::example::fetch::fetcher::Guest for Fetcher {
    async fn values() -> Result<ValueReport, String> {
        value_audit()
    }

    async fn exchange(spec: RequestSpec) -> Result<ExchangeReport, String> {
        let fields = Fields::from_list(&spec.fields).map_err(|error| format!("{error:?}"))?;
        let options = RequestOptions::new();
        options
            .set_connect_timeout(spec.connect)
            .map_err(|error| format!("{error:?}"))?;
        options
            .set_first_byte_timeout(spec.first_byte)
            .map_err(|error| format!("{error:?}"))?;
        options
            .set_between_bytes_timeout(spec.between_bytes)
            .map_err(|error| format!("{error:?}"))?;
        let (trailers, written) =
            wit_future::new::<Result<Option<Trailers>, ErrorCode>>(|| Ok(None));
        let trailer_error = spec.trailer_error;
        let contents = if spec.chunks.is_empty() {
            wit_bindgen::spawn_local(async move {
                let value = if spec.trailers.is_empty() {
                    None
                } else {
                    Some(Fields::from_list(&spec.trailers).unwrap())
                };
                let _ = trailers
                    .write(if trailer_error {
                        Err(ErrorCode::ConnectionWriteTimeout)
                    } else {
                        Ok(value)
                    })
                    .await;
            });
            None
        } else {
            let (mut writer, reader) = wit_stream::new::<u8>();
            wit_bindgen::spawn_local(async move {
                for chunk in spec.chunks {
                    if !writer.write_all(chunk).await.is_empty() {
                        break;
                    }
                }
                drop(writer);
                let value = if spec.trailers.is_empty() {
                    None
                } else {
                    Some(Fields::from_list(&spec.trailers).unwrap())
                };
                let _ = trailers
                    .write(if trailer_error {
                        Err(ErrorCode::ConnectionWriteTimeout)
                    } else {
                        Ok(value)
                    })
                    .await;
            });
            Some(reader)
        };
        let (request, sent) = Request::new(fields, contents, written, Some(options));
        request
            .set_method(&Method::Other(spec.method))
            .map_err(|()| "method")?;
        request
            .set_scheme(Some(&match spec.scheme.as_str() {
                "http" => Scheme::Http,
                "https" => Scheme::Https,
                _ => Scheme::Other(spec.scheme),
            }))
            .map_err(|()| "scheme")?;
        request
            .set_authority(Some(&spec.authority))
            .map_err(|()| "authority")?;
        request
            .set_path_with_query(Some(&spec.path))
            .map_err(|()| "path")?;
        let response = client::send(request)
            .await
            .map_err(|error| format!("{error:?}"))?;
        let status = response.get_status_code();
        let fields = response.get_headers().copy_all();
        let (body, trailers) = Response::consume_body(
            response,
            wit_future::new::<Result<(), ErrorCode>>(|| Ok(())).1,
        );
        let body = body.collect().await;
        let trailers = trailers
            .await
            .map(|fields| fields.map(|fields| fields.copy_all()).unwrap_or_default())
            .map_err(|error| format!("{error:?}"));
        let transmission = sent.await.map_err(|error| format!("{error:?}"));
        Ok(ExchangeReport {
            status,
            fields,
            body,
            trailers,
            transmission,
        })
    }

    async fn fetch(scheme: String, authority: String, path: String) -> Result<String, String> {
        let response = send(scheme, authority, path).await?;
        let status = response.get_status_code();
        let (_, consumed) = wit_future::new::<Result<(), ErrorCode>>(|| Ok(()));
        let (body, _trailers) = Response::consume_body(response, consumed);
        let bytes: Vec<u8> = body.collect().await;
        Ok(format!("{status} {}", String::from_utf8_lossy(&bytes)))
    }

    async fn first(scheme: String, authority: String, path: String) -> Result<String, String> {
        let response = send(scheme, authority, path).await?;
        let (_, consumed) = wit_future::new::<Result<(), ErrorCode>>(|| Ok(()));
        let (mut body, _trailers) = Response::consume_body(response, consumed);
        let (_, chunk) = body.read(Vec::with_capacity(4096)).await;
        // Dropping the reader ends the body before the peer finishes it.
        drop(body);
        Ok(String::from_utf8_lossy(&chunk).into_owned())
    }

    async fn keep(scheme: String, authority: String, path: String) -> Result<u16, String> {
        let response = send(scheme, authority, path).await?;
        let status = response.get_status_code();
        *KEPT.0.borrow_mut() = Some(response);
        Ok(status)
    }

    async fn hold_close(scheme: String, authority: String, path: String) -> Result<String, String> {
        let response = send(scheme, authority, path).await?;
        let (handled, consumed) = wit_future::new::<Result<(), ErrorCode>>(|| Ok(()));
        let (mut body, _trailers) = Response::consume_body(response, consumed);
        let (_, first) = body.read(Vec::with_capacity(4096)).await;
        // Close while the body is still held: only the handling result can end the connection.
        let _ = handled
            .write(Err(ErrorCode::InternalError(Some(
                "response closed".to_string(),
            ))))
            .await;
        let mut after = 0;
        loop {
            let (status, chunk) = body.read(Vec::with_capacity(4096)).await;
            after += chunk.len();
            if chunk.is_empty() && !matches!(status, wit_bindgen::StreamResult::Complete(_)) {
                break;
            }
        }
        drop(body);
        Ok(format!("{}|{after}", String::from_utf8_lossy(&first)))
    }

    async fn read_and_keep(
        scheme: String,
        authority: String,
        path: String,
    ) -> Result<String, String> {
        let response = send(scheme, authority, path).await?;
        let (_, consumed) = wit_future::new::<Result<(), ErrorCode>>(|| Ok(()));
        let (mut body, _trailers) = Response::consume_body(response, consumed);
        let (_, first) = body.read(Vec::with_capacity(4096)).await;
        *KEPT_BODY.0.borrow_mut() = Some(body);
        Ok(String::from_utf8_lossy(&first).into_owned())
    }

    async fn stream_held(
        scheme: String,
        authority: String,
        path: String,
    ) -> Result<String, String> {
        let response = send(scheme, authority, path).await?;
        let (handled, consumed) = wit_future::new::<Result<(), ErrorCode>>(|| Ok(()));
        let (body, _trailers) = Response::consume_body(response, consumed);
        let bytes: Vec<u8> = body.collect().await;
        drop(handled);
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn read_after_barrier(
        scheme: String,
        authority: String,
        path: String,
        barrier: String,
    ) -> Result<String, String> {
        let response = send(scheme.clone(), authority.clone(), path).await?;
        // Whatever the barrier request returns, the body above is read only afterwards.
        let _ = send(scheme, authority, barrier).await;
        let (_, consumed) = wit_future::new::<Result<(), ErrorCode>>(|| Ok(()));
        let (body, _trailers) = Response::consume_body(response, consumed);
        let bytes: Vec<u8> = body.collect().await;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn close_kept() -> Result<String, String> {
        let response = KEPT.0.borrow_mut().take().ok_or("no kept response")?;
        let (handled, consumed) = wit_future::new::<Result<(), ErrorCode>>(|| Ok(()));
        let (mut body, _trailers) = Response::consume_body(response, consumed);
        let (_, chunk) = body.read(Vec::with_capacity(4096)).await;
        drop(body);
        // Close the response: the host learns it through the response-handling future.
        let _ = handled
            .write(Err(ErrorCode::InternalError(Some(
                "response closed".to_string(),
            ))))
            .await;
        Ok(String::from_utf8_lossy(&chunk).into_owned())
    }
}

export!(Fetcher);
