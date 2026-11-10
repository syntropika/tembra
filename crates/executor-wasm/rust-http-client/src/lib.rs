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
}

export!(Fetcher);
