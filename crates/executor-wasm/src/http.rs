//! Outgoing standard `wasi:http` hooks shared by source and compiled Stores.
//!
//! Bodies, fields, errors and options stay upstream `Host` trait semantics. These hooks add
//! Tembra authority (the originating call and the exact granted origin), checked before any
//! resolver or connect work, and hand the request to the host transport.
use std::{future::Future, net::IpAddr, pin::Pin, sync::Arc};

mod transport;
pub(crate) use transport::{CallOwned, Commands, Exchange, Held, Ledger, Transport};
use wasmtime_wasi_http::{RequestOptions, WasiBody, WasiHttpHooks};

/// The one canonical interpretation of an outgoing request target. The grant is checked against
/// exactly this value and the transport resolves, connects, verifies TLS and writes the request
/// line and `Host` from it, so no second parser can read the raw URI differently.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Target {
    pub(crate) tls: bool,
    pub(crate) host: Host,
    pub(crate) port: u16,
    /// Origin-form request target (`/path?query`).
    pub(crate) path: http::uri::PathAndQuery,
}

/// A canonical target host: an IP literal, or a lowercase ASCII DNS name.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Host {
    Ip(IpAddr),
    Name(String),
}

impl Target {
    /// Validate an absolute `http`/`https` URI. Accepted hosts are strict IP literals (dotted
    /// quad, bracketed IPv6) or LDH DNS names, compared case-insensitively. Anything a WHATWG URL
    /// parser would reinterpret (percent escapes, non-ASCII, numeric shorthand such as `127.1` or
    /// `0x7f.1`, user info, empty or out-of-range ports) is refused before any I/O.
    pub(crate) fn parse(uri: &http::Uri) -> Result<Self, String> {
        let tls = match uri.scheme_str() {
            Some("https") => true,
            Some("http") => false,
            _ => return Err("HTTP request target must be an absolute http(s) URL".into()),
        };
        let authority = uri
            .authority()
            .ok_or("HTTP request target has no authority")?;
        let raw = authority.as_str();
        if raw.contains('@') {
            return Err("HTTP request target must not carry user info".into());
        }
        let host = authority.host();
        let host = if let Some(literal) = host.strip_prefix('[') {
            let literal = literal
                .strip_suffix(']')
                .ok_or("HTTP request host is malformed")?;
            Host::Ip(IpAddr::V6(
                literal
                    .parse()
                    .map_err(|_| "HTTP request host is not a valid IPv6 literal")?,
            ))
        } else if let Ok(ip) = host.parse::<std::net::Ipv4Addr>() {
            Host::Ip(IpAddr::V4(ip))
        } else {
            let name = host.to_ascii_lowercase();
            let labels: Vec<&str> = name.trim_end_matches('.').split('.').collect();
            let valid = !name.is_empty()
                && name.len() <= 253
                && labels.iter().all(|label| {
                    !label.is_empty()
                        && label.len() <= 63
                        && !label.starts_with('-')
                        && !label.ends_with('-')
                        && label
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                })
                // A numeric final label makes URL parsers read the host as an IPv4 address.
                && !labels
                    .last()
                    .is_some_and(|label| label.bytes().all(|byte| byte.is_ascii_digit())
                        || label.starts_with("0x"));
            if !valid {
                return Err("HTTP request host is not a canonical DNS name or IP literal".into());
            }
            Host::Name(name)
        };
        let port = match authority.port() {
            Some(port) if port.as_str().is_empty() => {
                return Err("HTTP request port is empty".into());
            }
            Some(port) => port.as_u16(),
            None if tls => 443,
            None => 80,
        };
        if port == 0 {
            return Err("HTTP request port is invalid".into());
        }
        let path = uri
            .path_and_query()
            .cloned()
            .unwrap_or_else(|| http::uri::PathAndQuery::from_static("/"));
        Ok(Self {
            tls,
            host,
            port,
            path,
        })
    }

    /// Host as written in URLs and `Host` (IPv6 bracketed).
    pub(crate) fn host_text(&self) -> String {
        match &self.host {
            Host::Ip(IpAddr::V6(ip)) => format!("[{ip}]"),
            Host::Ip(ip) => ip.to_string(),
            Host::Name(name) => name.clone(),
        }
    }

    /// The `Host` header value: the port is omitted only when it is the scheme default.
    pub(crate) fn authority(&self) -> String {
        let default = if self.tls { 443 } else { 80 };
        if self.port == default {
            self.host_text()
        } else {
            format!("{}:{}", self.host_text(), self.port)
        }
    }

    /// The canonical origin URL checked against grants.
    pub(crate) fn origin(&self) -> String {
        let scheme = if self.tls { "https" } else { "http" };
        format!("{scheme}://{}:{}/", self.host_text(), self.port)
    }
}

/// Why a request was refused before any transport work; each maps to its own standard code.
#[derive(Debug)]
pub(crate) enum Refusal {
    /// No admitted call issued it (an initializer or an unrelated task): `HTTP-request-denied`.
    Unattributed,
    /// The issuing call or callback has ended or was cancelled: `internal-error`.
    Ended(String),
}

impl Refusal {
    fn code(self) -> wasmtime_wasi_http::Error {
        match self {
            Refusal::Unattributed => wasmtime_wasi_http::Error::HttpRequestDenied,
            Refusal::Ended(reason) => wasmtime_wasi_http::Error::InternalError(Some(reason)),
        }
    }
}

/// Confirmation of a request's call authority, awaited before transport work starts.
pub(crate) type Confirmation = Pin<Box<dyn Future<Output = Result<(), Refusal>> + Send>>;

/// An admitted request: the confirmation awaited before transport work and, for a compiled
/// call, its work ledger and cancellation.
pub(crate) struct Admission {
    pub(crate) confirmation: Confirmation,
    pub(crate) call: Option<Ledger>,
    /// The originating call's cancellation (or end); a send still connecting or awaiting its
    /// response head is dropped when it resolves. A body the call reads is closed through the
    /// Store's command boundary (`Transport::commands`).
    pub(crate) cancelled: Option<Cancelled>,
}

/// A cloneable signal of a compiled call's cancellation or end.
pub(crate) type Cancelled = futures::future::Shared<Pin<Box<dyn Future<Output = ()> + Send>>>;

/// Who may send: the call that is running when upstream asks the hooks to send.
pub(crate) trait Authority: Send + Sync + 'static {
    /// Admit the current call synchronously, inside the guest call.
    fn admit(&self) -> Result<Admission, Refusal>;
}

/// The response future and connection driver upstream hooks return.
type Sent = Box<
    dyn Future<
            Output = Result<
                (
                    http::Response<WasiBody>,
                    Box<dyn Future<Output = Result<(), wasmtime_wasi_http::Error>> + Send>,
                ),
                wasmtime_wasi_http::Error,
            >,
        > + Send,
>;

/// Tembra hooks behind upstream's public `send_request` seam.
pub(crate) struct Hooks {
    grants: Arc<crate::Grants>,
    authority: Arc<dyn Authority>,
    transport: Transport,
}

impl Hooks {
    pub(crate) fn new(
        grants: Arc<crate::Grants>,
        authority: Arc<dyn Authority>,
        transport: Transport,
    ) -> Self {
        Self {
            grants,
            authority,
            transport,
        }
    }

    /// Exact origin grant plus call authority, both before any transport work. The grant is
    /// checked against the canonical target the transport then uses, never the raw URI.
    fn admit(
        &self,
        request: &http::Request<WasiBody>,
    ) -> Result<(Target, Admission), wasmtime_wasi_http::Error> {
        let admission = self.authority.admit().map_err(Refusal::code)?;
        let target = Target::parse(request.uri())
            .map_err(|_| wasmtime_wasi_http::Error::HttpRequestUriInvalid)?;
        crate::providers::check_http_origin(&self.grants, &target.origin(), &target.host_text())
            .map_err(|_| wasmtime_wasi_http::Error::HttpRequestDenied)?;
        Ok((target, admission))
    }
}

impl WasiHttpHooks for Hooks {
    fn send_request(
        &mut self,
        request: http::Request<WasiBody>,
        options: Option<RequestOptions>,
        response: Box<dyn Future<Output = Result<(), wasmtime_wasi_http::Error>> + Send>,
    ) -> Sent {
        let admitted = self.admit(&request);
        let transport = self.transport.clone();
        Box::new(async move {
            // Refusals are standard error codes for the guest, never traps, and happen before
            // any resolver or connect work.
            let (target, admission) = admitted?;
            let send = async {
                admission.confirmation.await.map_err(Refusal::code)?;
                let exchange = Exchange {
                    sending_call: admission.call,
                    handling: Box::into_pin(response),
                };
                transport.send(target, request, options, exchange).await
            };
            let (response, driver) = match admission.cancelled {
                None => send.await?,
                Some(cancelled) => {
                    let send = std::pin::pin!(send);
                    match futures::future::select(send, cancelled).await {
                        futures::future::Either::Left((sent, _)) => sent?,
                        // Dropping the send aborts its reactor connect; that work stays counted
                        // until they physically end, and the call waits for them.
                        futures::future::Either::Right(_) => {
                            return Err(wasmtime_wasi_http::Error::InternalError(Some(
                                "call cancelled".into(),
                            )));
                        }
                    }
                }
            };
            Ok((
                response,
                Box::new(driver) as Box<dyn Future<Output = _> + Send>,
            ))
        })
    }
}
