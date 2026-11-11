//! Configure providers once; each Orb separately selects mounted directories and access.
use crate::failure;
use std::{
    collections::{BTreeMap, BTreeSet},
    net::SocketAddr,
    path::PathBuf,
};
use tembra::Result;
use tembra_contracts::HostImport;
pub use wasmtime_wasi::{Deterministic, HostMonotonicClock, HostWallClock, Rng};
use wasmtime_wasi::{WasiCtxBuilder, filesystem::FsPerms, sockets::SocketAddrUse};

type Factory<T> = std::sync::Arc<dyn Fn() -> Box<T> + Send + Sync>;
type Apply = std::sync::Arc<dyn Fn(&mut WasiCtxBuilder) + Send + Sync>;

/// Standard clock and random sources of an engine. Every new Store (including a reload) gets
/// fresh instances; unset sources keep upstream defaults (host clocks, OS-seeded generators).
#[derive(Clone, Default)]
pub(crate) struct System {
    pub wall: Option<Factory<dyn HostWallClock>>,
    pub monotonic: Option<Factory<dyn HostMonotonicClock>>,
    pub secure: Option<Apply>,
    pub insecure: Option<Apply>,
    pub seed: Option<std::sync::Arc<dyn Fn() -> u128 + Send + Sync>>,
}

impl std::fmt::Debug for System {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("System")
    }
}

/// One clock instance of a Store, shared by upstream WASI and the interpreter's WASIp1
/// imports so the standard clock and source built-ins read the same configured source.
#[derive(Clone)]
struct SharedWall(std::sync::Arc<std::sync::Mutex<Box<dyn HostWallClock>>>);
impl HostWallClock for SharedWall {
    fn resolution(&self) -> std::time::Duration {
        self.0.lock().expect("wall clock").resolution()
    }
    fn now(&self) -> std::time::Duration {
        self.0.lock().expect("wall clock").now()
    }
}
#[derive(Clone)]
struct SharedMonotonic(std::sync::Arc<std::sync::Mutex<Box<dyn HostMonotonicClock>>>);
impl HostMonotonicClock for SharedMonotonic {
    fn resolution(&self) -> u64 {
        self.0.lock().expect("monotonic clock").resolution()
    }
    fn now(&self) -> u64 {
        self.0.lock().expect("monotonic clock").now()
    }
}

/// The system authority of one new Store, built once from its installation's grants.
pub(crate) struct Prepared {
    pub wasi: WasiCtxBuilder,
    /// The same clock pair as `wasi`, for the interpreter's WASIp1 imports; clocks grant only.
    pub clocks: Option<crate::worker::wasi::Clocks>,
    /// Built-in authority for a source Load; the seed equals `wasi`'s insecure seed.
    pub builtins: crate::protocol::Builtins,
    /// Whether the provider bound to `wasi:sockets/ip-name-lookup` grants name resolution.
    pub resolve: bool,
}

/// The per-Store clocks for WASIp1 and the Load built-ins.
type Shared = (
    Option<crate::worker::wasi::Clocks>,
    crate::protocol::Builtins,
);

impl System {
    /// Apply every configured provider whose capability is granted, invoking each factory at
    /// most once for this Store. Ungranted providers and CLI data are never touched.
    fn prepare(
        &self,
        grants: &Grants,
        cli: &crate::install::Cli,
        context: &mut WasiCtxBuilder,
    ) -> Result<Shared> {
        let mut builtins = crate::protocol::Builtins::default();
        let mut clocks = None;
        if grants.system.contains(&SystemCapability::Clocks) {
            let wall = SharedWall(std::sync::Arc::new(std::sync::Mutex::new(
                self.wall.as_ref().map_or_else(
                    || Box::new(wasmtime_wasi::clocks::WallClock::new()) as _,
                    |factory| factory(),
                ),
            )));
            let monotonic = SharedMonotonic(std::sync::Arc::new(std::sync::Mutex::new(
                self.monotonic.as_ref().map_or_else(
                    || Box::new(wasmtime_wasi::clocks::MonotonicClock::new()) as _,
                    |factory| factory(),
                ),
            )));
            context
                .wall_clock(wall.clone())
                .monotonic_clock(monotonic.clone());
            clocks = Some(crate::worker::wasi::Clocks {
                wall: Box::new(wall),
                monotonic: Box::new(monotonic),
            });
            builtins.clock = true;
        }
        if grants.system.contains(&SystemCapability::Random) {
            for apply in [&self.secure, &self.insecure].into_iter().flatten() {
                apply(context);
            }
            let seed = match &self.seed {
                Some(seed) => seed(),
                None => {
                    let mut random = wasmtime_wasi::thread_rng();
                    (u128::from(random.next_u64()) << 64) | u128::from(random.next_u64())
                }
            };
            context.insecure_random_seed(seed);
            // Canonical order on every backend: `[getInsecureSeed()[0], getInsecureSeed()[1]]`,
            // which upstream reports as `(low, high)` of the u128.
            builtins.seed = Some([(seed as u64).to_string(), ((seed >> 64) as u64).to_string()]);
        }
        if grants.system.contains(&SystemCapability::Cli) {
            cli_context(context, cli)?;
        }
        Ok((clocks, builtins))
    }
}

/// The status of a contained `wasi:cli/exit`, if `error` is one.
pub(crate) fn exit_status(error: &wasmtime::Error) -> Option<i32> {
    error
        .downcast_ref::<wasmtime_wasi::I32Exit>()
        .map(|exit| exit.0)
}

pub use tembra_contracts::{FilesystemGrant, Grants, HttpGrant, NetworkGrant, SystemCapability};

/// Concrete filesystem directories configured by the embedding host.
#[derive(Clone, Debug, Default)]
pub(crate) struct Providers {
    pub filesystem: BTreeMap<String, PathBuf>,
    pub network: BTreeSet<String>,
    pub system: System,
}

impl Providers {
    /// Validate standard imports and mount only providers explicitly granted to this Orb.
    pub(crate) fn context(
        &self,
        imports: &[HostImport],
        grants: &Grants,
        cli: &crate::install::Cli,
    ) -> Result<Prepared> {
        // The built-in network provider implements both sockets and HTTP.
        grants.check(
            imports,
            &self.filesystem.keys().cloned().collect(),
            &self.network,
            &self.network,
        )?;
        for grant in &grants.http {
            for origin in &grant.origins {
                http_origin(origin)?;
            }
        }
        let mut context = WasiCtxBuilder::new();
        let (clocks, builtins) = self.system.prepare(grants, cli, &mut context)?;
        // Each interface carries only the authority of the provider it is bound to.
        let sockets = Sockets::scoped(grants, bound(imports, "wasi:sockets/types@0.3.0"));
        let resolve = bound(imports, "wasi:sockets/ip-name-lookup@0.3.0").is_some_and(|provider| {
            grants
                .network
                .iter()
                .any(|grant| grant.provider == provider && grant.resolve)
        });
        // Name lookup is served only by `crate::dns`, never by upstream's untracked resolver.
        context
            .allow_tcp(sockets.tcp())
            .allow_udp(!sockets.datagrams.is_empty() || !sockets.bind.is_empty())
            .allow_ip_name_lookup(false);
        let sockets = std::sync::Arc::new(sockets);
        context.socket_addr_check(move |address, use_| {
            let allowed = sockets.allows(address, use_);
            Box::pin(async move { allowed })
        });
        for grant in &grants.filesystem {
            let directory = self.filesystem.get(&grant.provider).ok_or_else(|| {
                failure(format!("unknown filesystem provider {}", grant.provider))
            })?;
            let permissions = if grant.writable {
                FsPerms::ReadWrite
            } else {
                FsPerms::ReadOnly
            };
            context
                .preopened_dir(directory, &grant.mount, permissions)
                .map_err(failure)?;
        }
        Ok(Prepared {
            wasi: context,
            clocks,
            builtins,
            resolve,
        })
    }
}

/// Use the native host's standard URL parser; the browser uses its built-in URL API.
fn http_url(value: &str) -> Result<url::Url> {
    let url = url::Url::parse(value)
        .map_err(|error| tembra::Error::invalid("http_origin", error.to_string()))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(tembra::Error::invalid(
            "http_origin",
            "HTTP URLs require an http(s) origin without credentials or fragments",
        ));
    }
    Ok(url)
}

fn http_origin(value: &str) -> Result<url::Url> {
    let url = http_url(value)?;
    if url.path() != "/"
        || url.query().is_some()
        || url.host_str().is_some_and(|host| host.contains('*'))
    {
        return Err(tembra::Error::invalid(
            "http_origin",
            "HTTP grants require exact origins, without paths, queries or wildcards",
        ));
    }
    Ok(url)
}

/// HTTP origin authority remains independent of raw socket permissions.
pub(crate) fn check_http_url(grants: &Grants, value: &str) -> Result<()> {
    let origin = http_url(value)?.origin();
    for grant in &grants.http {
        for allowed in &grant.origins {
            if http_origin(allowed)?.origin() == origin {
                return Ok(());
            }
        }
    }
    Err(tembra::Error::new(
        tembra::core::Code::Forbidden,
        "orb_grant",
        "HTTP origin is not granted to this Orb",
    ))
}

/// Check a canonical standard `wasi:http` origin (see `crate::http::Target`). The canonical
/// form is a fixed point of URL normalization; anything the URL parser would reinterpret is
/// refused rather than checked under a second interpretation.
pub(crate) fn check_http_origin(grants: &Grants, origin: &str, host: &str) -> Result<()> {
    let url = http_url(origin)?;
    if url.host_str() != Some(host) {
        return Err(tembra::Error::invalid(
            "http_origin",
            "HTTP request host is not in canonical form",
        ));
    }
    check_http_url(grants, origin)
}

/// The provider an Orb bound to a standard interface, if it imports it.
fn bound<'a>(imports: &'a [HostImport], contract: &str) -> Option<&'a str> {
    imports
        .iter()
        .find(|import| import.contract == contract)
        .map(|import| import.provider.as_str())
}

/// Exact socket authority of the network grants of the provider bound to `wasi:sockets/types`.
///
/// HTTP never uses these: `wasi:http` requests are checked against HTTP origin grants instead.
#[derive(Default)]
struct Sockets {
    /// Remote TCP peers.
    connect: BTreeSet<SocketAddr>,
    /// Local addresses a TCP socket may listen on; any client may then be accepted.
    listen: BTreeSet<SocketAddr>,
    /// Local addresses an explicit TCP or UDP bind may use.
    bind: BTreeSet<SocketAddr>,
    /// Remote UDP peers for sending and receiving datagrams.
    datagrams: BTreeSet<SocketAddr>,
}

impl Sockets {
    /// Authority of `provider`'s grants only; other providers' grants are never merged in.
    fn scoped(grants: &Grants, provider: Option<&str>) -> Self {
        let mut sockets = Self::default();
        for grant in grants
            .network
            .iter()
            .filter(|grant| Some(grant.provider.as_str()) == provider)
        {
            sockets.connect.extend(grant.connect.iter().copied());
            sockets.listen.extend(grant.listen.iter().copied());
            sockets.bind.extend(grant.bind.iter().copied());
            sockets.datagrams.extend(grant.datagrams.iter().copied());
        }
        sockets
    }

    fn tcp(&self) -> bool {
        !(self.connect.is_empty() && self.listen.is_empty() && self.bind.is_empty())
    }

    /// An implicit bind by the OS uses the wildcard address and an ephemeral port.
    fn implicit(address: SocketAddr) -> bool {
        address.ip().is_unspecified() && address.port() == 0
    }

    fn allows(&self, address: SocketAddr, use_: SocketAddrUse) -> bool {
        match use_ {
            SocketAddrUse::TcpConnect => self.connect.contains(&address),
            SocketAddrUse::TcpBind => {
                (Self::implicit(address) && self.tcp())
                    || self.bind.contains(&address)
                    || self.listen.contains(&address)
            }
            SocketAddrUse::TcpListen => self.listen.iter().any(|granted| {
                granted.ip() == address.ip()
                    && (granted.port() == 0 || granted.port() == address.port())
            }),
            SocketAddrUse::TcpAccept => !self.listen.is_empty(),
            SocketAddrUse::UdpBind => {
                (Self::implicit(address) && !self.datagrams.is_empty())
                    || self.bind.contains(&address)
            }
            SocketAddrUse::UdpSend | SocketAddrUse::UdpReceive => self.datagrams.contains(&address),
        }
    }
}

/// Apply one Orb's CLI data and fresh streams; nothing is inherited from the host process.
fn cli_context(context: &mut WasiCtxBuilder, cli: &crate::install::Cli) -> Result<()> {
    context.args(&cli.arguments).envs(&cli.environment);
    if let Some(cwd) = &cli.initial_cwd {
        context.initial_cwd(cwd);
    }
    if let Some(stdin) = crate::stdio::stdin(&cli.stdin)? {
        context.stdin(stdin);
    }
    if let Some(stdout) = crate::stdio::stdout(&cli.stdout)? {
        context.stdout(stdout);
    }
    if let Some(stderr) = crate::stdio::stdout(&cli.stderr)? {
        context.stderr(stderr);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_origins_use_standard_normalization_without_widening_authority() {
        let grants = Grants {
            http: vec![HttpGrant {
                provider: "web".into(),
                origins: vec!["https://EXAMPLE.com:443".into(), "http://[::1]:8000".into()],
            }],
            ..Default::default()
        };
        check_http_url(&grants, "https://example.com/api?q=1").unwrap();
        check_http_url(&grants, "http://[::1]:8000/api").unwrap();
        for url in [
            "http://example.com/",
            "https://example.com:444/",
            "https://example.com.attacker.test/",
            "https://user@example.com/",
            "https://example.com/#",
        ] {
            assert!(check_http_url(&grants, url).is_err(), "{url}");
        }
        for origin in [
            "https://*.example.com",
            "https://example.com/path",
            "https://example.com/?",
            "https://example.com/#",
            "file:///tmp",
            "https://user:password@example.com/",
        ] {
            assert!(http_origin(origin).is_err(), "{origin}");
        }
    }

    fn address(text: &str) -> SocketAddr {
        text.parse().unwrap()
    }

    #[test]
    fn socket_grants_admit_only_their_own_uses() {
        let grants = Grants {
            network: vec![NetworkGrant {
                provider: "net".into(),
                connect: vec![address("10.0.0.1:443")],
                listen: vec![address("127.0.0.1:8080")],
                datagrams: vec![address("10.0.0.2:53")],
                ..NetworkGrant::default()
            }],
            ..Grants::default()
        };
        let sockets = Sockets::scoped(&grants, Some("net"));
        for (text, use_, allowed) in [
            ("10.0.0.1:443", SocketAddrUse::TcpConnect, true),
            ("10.0.0.1:80", SocketAddrUse::TcpConnect, false),
            ("0.0.0.0:0", SocketAddrUse::TcpBind, true),
            ("127.0.0.1:8080", SocketAddrUse::TcpBind, true),
            ("127.0.0.1:9090", SocketAddrUse::TcpBind, false),
            ("127.0.0.1:8080", SocketAddrUse::TcpListen, true),
            ("0.0.0.0:0", SocketAddrUse::TcpListen, false),
            ("10.9.9.9:1234", SocketAddrUse::TcpAccept, true),
            ("0.0.0.0:0", SocketAddrUse::UdpBind, true),
            ("10.0.0.2:53", SocketAddrUse::UdpSend, true),
            ("10.0.0.3:53", SocketAddrUse::UdpSend, false),
            ("10.0.0.3:53", SocketAddrUse::UdpReceive, false),
        ] {
            assert_eq!(sockets.allows(address(text), use_), allowed, "{text}");
        }
        // Another provider's grants are never merged into the bound provider's authority.
        let other = Sockets::scoped(&grants, Some("other"));
        assert!(!other.tcp() && !other.allows(address("10.0.0.1:443"), SocketAddrUse::TcpConnect));
        assert!(!Sockets::scoped(&grants, None).tcp());
        // Without grants nothing is admitted, not even implicit binds.
        let none = Sockets::scoped(&Grants::default(), Some("net"));
        assert!(!none.tcp() && !none.allows(address("0.0.0.0:0"), SocketAddrUse::TcpBind));
        assert!(!none.allows(address("0.0.0.0:0"), SocketAddrUse::UdpBind));
        assert!(!none.allows(address("1.2.3.4:5"), SocketAddrUse::TcpAccept));
    }
}
