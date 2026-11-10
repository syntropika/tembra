//! Source-Orb HTTP through the host's standard WASI HTTP client.
//!
//! DNS, TLS and HTTP/1 framing are upstream `wasmtime_wasi_http::default_send_request`; this
//! module only checks the request against the Orb's grants, sends it on the shared reactor and
//! exposes the response body incrementally. Response bodies are entries of the Orb's [`Streams`]
//! table, so they share its identities, limit and callback epochs with raw sockets.
use super::streams::{Reservation, Streams};
use crate::{
    failure,
    protocol::{HTTP_BODY_BYTES, HTTP_HEADER_LIMIT, HttpResponse, TRANSPORT_CHUNK_BYTES},
};
use bytes::{Buf, Bytes};
use http_body_util::BodyExt;
use serde_json::Value;
use std::{sync::Arc, time::Instant};
use tembra::Result;
use tokio::sync::{Mutex as Exclusive, OwnedMutexGuard, watch};
use wasmtime_wasi_http::{RequestOptions, WasiBody};

/// One checked outgoing request, ready to run on the reactor.
pub(super) struct Outgoing(http::Request<Bytes>);

impl Outgoing {
    /// Check one guest request against the granted origins before any connection is made.
    ///
    /// The URL is normalized once; the grant check and the request see the same serialization.
    pub(super) fn new(
        grants: &crate::Grants,
        url: &str,
        method: &str,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> Result<Self> {
        let url = url::Url::parse(url).map_err(|error| failure(format!("invalid URL: {error}")))?;
        crate::providers::check_http_url(grants, url.as_str())?;
        if body.len() > HTTP_BODY_BYTES {
            return Err(failure("HTTP request body exceeds the one mebibyte limit"));
        }
        if headers.len() > HTTP_HEADER_LIMIT {
            return Err(failure("HTTP request has too many headers"));
        }
        let uri: http::Uri = url
            .as_str()
            .parse()
            .map_err(|error| failure(format!("invalid HTTP URL: {error}")))?;
        let authority = uri
            .authority()
            .ok_or_else(|| failure("HTTP URLs require an authority"))?
            .clone();
        let mut request = http::Request::builder()
            .method(
                http::Method::from_bytes(method.as_bytes())
                    .map_err(|_| failure("invalid HTTP method"))?,
            )
            .uri(uri)
            .body(Bytes::from(body))
            .map_err(failure)?;
        let fields = request.headers_mut();
        for (name, value) in headers {
            let name = http::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| failure("invalid HTTP header name"))?;
            // The host owns connection framing, like standard guests' forbidden fields.
            if wasmtime_wasi_http::DEFAULT_FORBIDDEN_HEADERS.contains(&name)
                || name == http::header::CONTENT_LENGTH
            {
                return Err(failure(format!("HTTP header {name} is set by the host")));
            }
            let value = http::HeaderValue::from_str(&value)
                .map_err(|_| failure("invalid HTTP header value"))?;
            fields.append(name, value);
        }
        // The standard sender writes an origin-form request line, so the host supplies `Host`.
        fields.insert(
            http::header::HOST,
            http::HeaderValue::from_str(authority.as_str()).map_err(failure)?,
        );
        Ok(Self(request))
    }

    /// Send within the operation deadline; the callback's end abandons the attempt.
    pub(super) async fn send(self, reservation: Reservation, deadline: Instant) -> Result<Value> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let options = RequestOptions {
            connect_timeout: Some(remaining),
            first_byte_timeout: Some(remaining),
            // Reads apply their own per-operation deadline.
            between_bytes_timeout: None,
        };
        let request = self
            .0
            .map(|body| http_body_util::Full::new(body).map_err(|never| match never {}));
        let sent = tokio::time::timeout_at(
            deadline.into(),
            wasmtime_wasi_http::default_send_request(request, Some(options)),
        );
        let (response, connection) = tokio::select! {
            sent = sent => sent
                .map_err(|_| failure("host I/O deadline exceeded"))?
                .map_err(|error| failure(format!("HTTP request failed: {error}")))?,
            () = reservation.abandoned() => {
                return Err(failure("callback ended before the HTTP response arrived"));
            }
        };
        // The connection keeps running while the body is read; closing the body aborts it.
        let driver = Driver(tokio::spawn(async move {
            let _ = connection.await;
        }));
        let (head, body) = response.into_parts();
        let id = reservation.register_body(|epoch| Body {
            epoch,
            closed: watch::Sender::new(false),
            reader: Arc::new(Exclusive::new(Reader {
                body: body.boxed_unsync(),
                pending: Bytes::new(),
                finished: false,
                _driver: driver,
            })),
        })?;
        let headers = head
            .headers
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
            .collect();
        serde_json::to_value(HttpResponse {
            id,
            status: head.status.as_u16(),
            headers,
        })
        .map_err(failure)
    }
}

/// Abort the connection task when its body is dropped.
struct Driver(tokio::task::JoinHandle<()>);

impl Drop for Driver {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// An HTTP response body issued to one callback epoch.
pub(super) struct Body {
    pub(super) epoch: u64,
    closed: watch::Sender<bool>,
    reader: Arc<Exclusive<Reader>>,
}

pub(super) struct Reader {
    body: WasiBody,
    /// Bytes of a received frame not yet returned to the guest.
    pending: Bytes,
    finished: bool,
    _driver: Driver,
}

impl Body {
    pub(super) fn close(&self) {
        self.closed.send_replace(true);
    }

    /// Claim the body; a second concurrent read is refused.
    pub(super) fn reader(&self) -> Result<OwnedMutexGuard<Reader>> {
        self.reader
            .clone()
            .try_lock_owned()
            .map_err(|_| failure("an HTTP body read is already pending"))
    }

    /// Return at most one chunk of the body as it arrives, or `null` at its end.
    pub(super) async fn read(
        self: Arc<Self>,
        mut reader: OwnedMutexGuard<Reader>,
        deadline: Instant,
    ) -> Result<Value> {
        let mut closed = self.closed.subscribe();
        while reader.pending.is_empty() && !reader.finished {
            let frame = tokio::select! {
                frame = tokio::time::timeout_at(deadline.into(), reader.body.frame()) => frame
                    .map_err(|_| failure("host I/O deadline exceeded"))?,
                _ = closed.wait_for(|closed| *closed) => {
                    return Err(failure("HTTP response body closed"));
                }
            };
            match frame {
                // Trailers are not part of the source body interface.
                Some(frame) => {
                    if let Ok(data) = frame
                        .map_err(|error| failure(format!("HTTP response body failed: {error}")))?
                        .into_data()
                    {
                        reader.pending = data;
                    }
                }
                None => reader.finished = true,
            }
        }
        if reader.pending.is_empty() {
            return Ok(Value::Null);
        }
        let count = reader.pending.len().min(TRANSPORT_CHUNK_BYTES);
        let chunk = reader.pending.copy_to_bytes(count);
        Ok(Value::Array(
            chunk.iter().map(|byte| Value::from(*byte)).collect(),
        ))
    }
}

impl Streams {
    /// Resolve and claim a response body issued to the active callback.
    pub(super) fn http_body(&self, id: u32) -> Result<(Arc<Body>, OwnedMutexGuard<Reader>)> {
        let body = self.body(id)?;
        let reader = body.reader()?;
        Ok((body, reader))
    }
}
