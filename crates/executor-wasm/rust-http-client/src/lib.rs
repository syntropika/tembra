//! A compiled Orb using the standard `wasi:http` client import and its resources directly.
//!
//! It is `no_std`, like the probe, so its component imports exactly its WIT world.
#![no_std]
extern crate alloc;

use alloc::{
    format,
    string::{String, ToString},
    vec::Vec,
};
use wasi::http::{
    client,
    types::{ErrorCode, Fields, Request, Response, Scheme, Trailers},
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

impl exports::example::fetch::fetcher::Guest for Fetcher {
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
            .write(Err(ErrorCode::InternalError(Some("response closed".to_string()))))
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

    async fn read_and_keep(scheme: String, authority: String, path: String) -> Result<String, String> {
        let response = send(scheme, authority, path).await?;
        let (_, consumed) = wit_future::new::<Result<(), ErrorCode>>(|| Ok(()));
        let (mut body, _trailers) = Response::consume_body(response, consumed);
        let (_, first) = body.read(Vec::with_capacity(4096)).await;
        *KEPT_BODY.0.borrow_mut() = Some(body);
        Ok(String::from_utf8_lossy(&first).into_owned())
    }

    async fn stream_held(scheme: String, authority: String, path: String) -> Result<String, String> {
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
            .write(Err(ErrorCode::InternalError(Some("response closed".to_string()))))
            .await;
        Ok(String::from_utf8_lossy(&chunk).into_owned())
    }
}

export!(Fetcher);
