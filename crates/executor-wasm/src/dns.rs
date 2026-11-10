//! Standard `wasi:sockets/ip-name-lookup` on Tembra's bounded, tracked resolver.
//!
//! Upstream's implementation runs `tokio::net::lookup_host` jobs nobody tracks after a call is
//! dropped. This implementation of the same standard trait keeps upstream's name semantics
//! (`url::Host` parsing with IDNA to punycode, IP literals answered without a lookup, bare IPv6
//! accepted, canonical addresses) and differs only in authority and ownership:
//! - a lookup the Orb's bound name-lookup provider does not grant fails with `access-denied`;
//! - each lookup runs on the reactor's bounded DNS workers (4 running, 60 queued), and a full
//!   resolver fails with `temporary-resolver-failure` instead of queueing without bound;
//! - each lookup is counted by the Store's work ledger and the current command's ledger until it
//!   physically returns, so callback retirement, compiled calls and revision teardown wait for
//!   it even when the guest's task was aborted.
use std::{net::IpAddr, str::FromStr};
use wasmtime::component::{Accessor, HasData};
use wasmtime_wasi::p3::bindings::sockets::{
    ip_name_lookup::{ErrorCode, Host, HostWithStore},
    types::IpAddress,
};

/// Accessor data type of the Tembra name-lookup implementation.
pub(crate) struct Dns;

impl HasData for Dns {
    type Data<'a> = Lookup<'a>;
}

/// What one Store's lookups need: the bound provider's grant and the Store's work ledgers.
pub(crate) struct Lookup<'a> {
    pub(crate) allowed: bool,
    pub(crate) transport: &'a crate::http::Transport,
}

impl Host for Lookup<'_> {}

/// Stores that serve standard name lookups.
pub(crate) trait DnsView: Send {
    fn dns(&mut self) -> Lookup<'_>;
}

impl<U: 'static> HostWithStore<U> for Dns {
    async fn resolve_addresses(
        store: &Accessor<U, Self>,
        name: String,
    ) -> wasmtime::Result<Result<Vec<IpAddress>, ErrorCode>> {
        let lookup = store.with(|mut view| {
            let view = view.get();
            resolve(view.allowed, view.transport, name)
        });
        Ok(lookup.await)
    }
}

/// Parse and admit synchronously, inside the guest call, then resolve.
fn resolve(
    allowed: bool,
    transport: &crate::http::Transport,
    name: String,
) -> impl Future<Output = Result<Vec<IpAddress>, ErrorCode>> + Send + use<> {
    let parsed = if allowed {
        parse(&name)
    } else {
        Err(ErrorCode::AccessDenied)
    };
    // The unit is taken now so the lookup is charged to the command that started it.
    let job = parsed
        .as_ref()
        .is_ok_and(|host| matches!(host, Parsed::Domain(_)))
        .then(|| transport.commands.hold_current());
    async move {
        let domain = match parsed? {
            Parsed::Ip(ip) => return Ok(vec![ip.into()]),
            Parsed::Domain(domain) => domain,
        };
        let job = job.expect("taken for every domain lookup");
        let lookup =
            crate::io::resolve(domain, 0, job).map_err(|_| ErrorCode::TemporaryResolverFailure)?;
        // Dropping the join handle (an aborted call) leaves the lookup and its unit running.
        let addresses = lookup
            .await
            .map_err(|_| ErrorCode::TemporaryResolverFailure)?
            .map_err(|_| ErrorCode::NameUnresolvable)?;
        Ok(addresses
            .into_iter()
            .map(|address| address.ip().to_canonical().into())
            .collect())
    }
}

#[derive(Debug, PartialEq)]
enum Parsed {
    Ip(IpAddr),
    Domain(String),
}

/// Upstream's name rules: a valid domain (unicode converted to punycode) or an IP literal.
fn parse(name: &str) -> Result<Parsed, ErrorCode> {
    match url::Host::parse(name) {
        Ok(url::Host::Domain(domain)) => Ok(Parsed::Domain(domain)),
        Ok(url::Host::Ipv4(address)) => Ok(Parsed::Ip(IpAddr::V4(address))),
        Ok(url::Host::Ipv6(address)) => Ok(Parsed::Ip(IpAddr::V6(address))),
        // `url::Host::parse` does not accept bare IPv6 addresses without brackets.
        Err(_) if let Ok(address) = std::net::Ipv6Addr::from_str(name) => {
            Ok(Parsed::Ip(IpAddr::V6(address)))
        }
        Err(_) => Err(ErrorCode::InvalidArgument),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_upstream_parsing() {
        let parsed = |name: &str| parse(name).ok();
        assert_eq!(
            parsed("127.0.0.1"),
            Some(Parsed::Ip("127.0.0.1".parse().unwrap()))
        );
        assert_eq!(parsed("::1"), Some(Parsed::Ip("::1".parse().unwrap())));
        assert_eq!(parsed("[::1]"), Some(Parsed::Ip("::1".parse().unwrap())));
        assert_eq!(
            parsed("Example.COM"),
            Some(Parsed::Domain("example.com".into()))
        );
        assert_eq!(
            parsed("bücher.example"),
            Some(Parsed::Domain("xn--bcher-kva.example".into()))
        );
        assert!(matches!(parse("a b"), Err(ErrorCode::InvalidArgument)));
        assert!(matches!(parse(""), Err(ErrorCode::InvalidArgument)));
    }

    #[test]
    fn denial_precedes_parsing_and_resolver_work() {
        let transport = crate::http::Transport::default();
        let lookup = resolve(false, &transport, "example.com".into());
        assert_eq!(transport.store_work.live(), 0);
        assert!(matches!(
            futures::executor::block_on(lookup),
            Err(ErrorCode::AccessDenied)
        ));
        // Literals never reach the resolver.
        let literal = resolve(true, &transport, "10.0.0.1".into());
        assert_eq!(transport.store_work.live(), 0);
        assert_eq!(futures::executor::block_on(literal).unwrap().len(), 1);
    }
}
