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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NetworkGrant {
    pub provider: String,
    pub connect: Vec<SocketAddr>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Grants {
    pub filesystem: Vec<FilesystemGrant>,
    pub network: Vec<NetworkGrant>,
}

impl Grants {
    /// Require declared imports, configured providers and normalized mounts before attaching I/O.
    pub fn check(
        &self,
        imports: &[HostImport],
        filesystem: &BTreeSet<String>,
        network: &BTreeSet<String>,
    ) -> Result<()> {
        for import in imports {
            let granted = match import.contract.as_str() {
                "wasi:filesystem/types@0.3.0" | "wasi:filesystem/preopens@0.3.0" => self
                    .filesystem
                    .iter()
                    .any(|grant| grant.provider == import.provider),
                "wasi:sockets/types@0.3.0" => self
                    .network
                    .iter()
                    .any(|grant| grant.provider == import.provider),
                "wasi:clocks/system-clock@0.3.0" if import.provider == "system-clock" => true,
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
                import.provider == grant.provider && import.contract == "wasi:sockets/types@0.3.0"
            }) {
                return Err(Error::new(
                    Code::Invalid,
                    "wit_import",
                    format!(
                        "network provider {} requires a wasi:sockets/types import",
                        grant.provider
                    ),
                ));
            }
        }
        Ok(())
    }
}
