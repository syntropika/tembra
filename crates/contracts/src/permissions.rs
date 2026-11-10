//! Pure permission checks shared by actual filesystem and socket adapters.
use crate::HostImport;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, net::SocketAddr};
use tembra_core::{Code, Error, Result};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FilesystemGrant {
    pub provider: String,
    pub mount: String,
    pub writable: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NetworkGrant {
    pub provider: String,
    #[serde(default)]
    pub connect: Vec<SocketAddr>,
    #[serde(default)]
    pub listen: Vec<SocketAddr>,
    #[serde(default)]
    pub bind: Vec<SocketAddr>,
    #[serde(default)]
    pub datagrams: Vec<SocketAddr>,
    #[serde(default)]
    pub resolve: bool,
}

/// Standard process-local capabilities explicitly granted to one Orb.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SystemCapability {
    Clocks,
    Random,
    Cli,
}

/// Recognize the process-local interfaces supplied by the pinned WASI runtime.
pub(crate) fn system_capability(contract: &str) -> Option<SystemCapability> {
    let capability = match contract.strip_suffix("@0.3.0")? {
        "wasi:clocks/monotonic-clock" | "wasi:clocks/system-clock" => SystemCapability::Clocks,
        "wasi:random/random" | "wasi:random/insecure" | "wasi:random/insecure-seed" => {
            SystemCapability::Random
        }
        "wasi:cli/environment"
        | "wasi:cli/exit"
        | "wasi:cli/stdin"
        | "wasi:cli/stdout"
        | "wasi:cli/stderr"
        | "wasi:cli/types"
        | "wasi:cli/terminal-input"
        | "wasi:cli/terminal-output"
        | "wasi:cli/terminal-stdin"
        | "wasi:cli/terminal-stdout"
        | "wasi:cli/terminal-stderr" => SystemCapability::Cli,
        _ => return None,
    };
    Some(capability)
}

/// An HTTP capability grants exact origins, independently of raw socket access.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HttpGrant {
    pub provider: String,
    pub origins: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Grants {
    pub filesystem: Vec<FilesystemGrant>,
    pub network: Vec<NetworkGrant>,
    pub http: Vec<HttpGrant>,
    pub system: BTreeSet<SystemCapability>,
}

impl Grants {
    /// Require the grant for one standard WASI instance that a compiled component binary
    /// actually imports with at least one function. Checked against the binary, never against
    /// artifact metadata, so a forged type-only claim cannot unlock an ambient capability.
    /// Versions are ignored because the upstream linker also satisfies semver-compatible
    /// imports with the pinned implementation.
    ///
    /// `serves_http` states that the same binary exports the incoming HTTP handler. Its request
    /// and response types are then admitted without an outgoing grant; `wasi:http/client` never is.
    pub fn check_component_import(&self, name: &str, serves_http: bool) -> Result<()> {
        let interface = name
            .split_once('@')
            .map_or(name, |(interface, _)| interface);
        let granted = match interface {
            "wasi:filesystem/types" | "wasi:filesystem/preopens" => !self.filesystem.is_empty(),
            "wasi:sockets/types" | "wasi:sockets/ip-name-lookup" => !self.network.is_empty(),
            "wasi:http/types" => serves_http || !self.http.is_empty(),
            "wasi:http/client" => !self.http.is_empty(),
            other => match system_capability(&format!("{other}@0.3.0")) {
                Some(capability) => self.system.contains(&capability),
                None => {
                    return Err(Error::new(
                        Code::Unsupported,
                        "wasi_contract",
                        format!("component imports unsupported standard interface {name}"),
                    ));
                }
            },
        };
        if granted {
            Ok(())
        } else {
            Err(Error::new(
                Code::Forbidden,
                "orb_grant",
                format!("component imports {name} without an Orb grant"),
            ))
        }
    }

    /// Require declared imports, configured providers and normalized mounts before attaching I/O.
    /// Each grant kind names providers of its own registry: socket grants `network`, HTTP grants
    /// `http`. A host whose provider implements both passes the same keys twice; the grants stay
    /// independent either way.
    pub fn check(
        &self,
        imports: &[HostImport],
        filesystem: &BTreeSet<String>,
        network: &BTreeSet<String>,
        http: &BTreeSet<String>,
    ) -> Result<()> {
        // The incoming HTTP provider is selected by planning only; no installer grant may claim
        // its name, whatever kind of grant it is.
        let reserved = self
            .filesystem
            .iter()
            .map(|grant| &grant.provider)
            .chain(self.network.iter().map(|grant| &grant.provider))
            .chain(self.http.iter().map(|grant| &grant.provider))
            .any(|provider| provider == crate::lower::INGRESS_PROVIDER);
        if reserved {
            return Err(Error::new(
                Code::Invalid,
                "host_provider",
                format!(
                    "{} is reserved for incoming HTTP and cannot be granted",
                    crate::lower::INGRESS_PROVIDER
                ),
            ));
        }
        for import in imports {
            let granted = match import.contract.as_str() {
                "wasi:filesystem/types@0.3.0" | "wasi:filesystem/preopens@0.3.0" => self
                    .filesystem
                    .iter()
                    .any(|grant| grant.provider == import.provider),
                "wasi:sockets/types@0.3.0" | "wasi:sockets/ip-name-lookup@0.3.0" => self
                    .network
                    .iter()
                    .any(|grant| grant.provider == import.provider),
                // Planning admits the ingress provider only for a handler world's types.
                "wasi:http/types@0.3.0" if import.provider == crate::lower::INGRESS_PROVIDER => {
                    true
                }
                "wasi:http/types@0.3.0" | "wasi:http/client@0.3.0" => self
                    .http
                    .iter()
                    .any(|grant| grant.provider == import.provider),
                contract if system_capability(contract).is_some() => {
                    self.system.contains(&system_capability(contract).unwrap())
                }
                _ => {
                    return Err(Error::new(
                        Code::Unsupported,
                        "wasi_contract",
                        format!(
                            "{}: unsupported host contract {} from provider {}",
                            import.binding, import.contract, import.provider
                        ),
                    ));
                }
            };
            if !granted {
                return Err(Error::new(
                    Code::Forbidden,
                    "orb_grant",
                    format!(
                        "{}: provider {} has no Orb grant",
                        import.binding, import.provider
                    ),
                ));
            }
        }
        let mut mounts = BTreeSet::new();
        for grant in &self.filesystem {
            if !filesystem.contains(&grant.provider) {
                return Err(Error::new(
                    Code::Invalid,
                    "host_provider",
                    format!("unknown filesystem provider {}", grant.provider),
                ));
            }
            if !imports.iter().any(|import| {
                import.provider == grant.provider
                    && import.contract == "wasi:filesystem/preopens@0.3.0"
            }) {
                return Err(Error::new(
                    Code::Invalid,
                    "wit_import",
                    format!(
                        "filesystem provider {} requires a wasi:filesystem/preopens import",
                        grant.provider
                    ),
                ));
            }
            if !grant.mount.starts_with('/')
                || grant.mount == "/"
                || grant
                    .mount
                    .split('/')
                    .skip(1)
                    .any(|part| part.is_empty() || part == "." || part == "..")
                || !mounts.insert(&grant.mount)
            {
                return Err(Error::new(
                    Code::Invalid,
                    "filesystem_mount",
                    format!(
                        "{}: mount {} must be normalized, absolute and unique",
                        grant.provider, grant.mount
                    ),
                ));
            }
        }
        for grant in &self.network {
            if !network.contains(&grant.provider) {
                return Err(Error::new(
                    Code::Invalid,
                    "host_provider",
                    format!("unknown network provider {}", grant.provider),
                ));
            }
            if !imports.iter().any(|import| {
                import.provider == grant.provider
                    && matches!(
                        import.contract.as_str(),
                        "wasi:sockets/types@0.3.0" | "wasi:sockets/ip-name-lookup@0.3.0"
                    )
            }) {
                return Err(Error::new(
                    Code::Invalid,
                    "wit_import",
                    format!(
                        "network provider {} requires a wasi:sockets import",
                        grant.provider
                    ),
                ));
            }
        }
        for grant in &self.http {
            if grant.origins.is_empty() || grant.provider.trim().is_empty() {
                return Err(Error::new(
                    Code::Invalid,
                    "http_origin",
                    "HTTP grants require a provider and at least one origin",
                ));
            }
            if !http.contains(&grant.provider) {
                return Err(Error::new(
                    Code::Invalid,
                    "host_provider",
                    format!("unknown HTTP provider {}", grant.provider),
                ));
            }
            if !imports.iter().any(|import| {
                import.provider == grant.provider && import.contract == "wasi:http/client@0.3.0"
            }) {
                return Err(Error::new(
                    Code::Invalid,
                    "wit_import",
                    "HTTP grants require a wasi:http/client import",
                ));
            }
        }
        Ok(())
    }
}
