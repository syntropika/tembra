//! Configure providers once; each Orb separately selects mounted directories and access.
use crate::failure;
use std::{
    collections::{BTreeMap, BTreeSet},
    net::SocketAddr,
    path::PathBuf,
};
use tembra::Result;
use tembra_contracts::HostImport;
use wasmtime_wasi::{WasiCtxBuilder, filesystem::FsPerms};

/// Filesystem authority granted to one Orb revision under a virtual path.
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FilesystemGrant {
    pub provider: String,
    pub mount: String,
    pub writable: bool,
}

/// Installation supplies grants independently of the WIT import declarations.
#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Grants {
    pub filesystem: Vec<FilesystemGrant>,
    pub network: Vec<NetworkGrant>,
}

/// Outbound TCP authority selects exact destinations and never permits listening or DNS.
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NetworkGrant {
    pub provider: String,
    pub connect: Vec<SocketAddr>,
}

/// Concrete filesystem directories configured by the embedding host.
#[derive(Clone, Debug, Default)]
pub(crate) struct Providers {
    pub filesystem: BTreeMap<String, PathBuf>,
    pub network: BTreeSet<String>,
}

impl Providers {
    /// Validate standard imports and mount only providers explicitly granted to this Orb.
    pub(crate) fn context(
        &self,
        imports: &[HostImport],
        grants: &Grants,
    ) -> Result<WasiCtxBuilder> {
        for import in imports {
            match import.contract.as_str() {
                "wasi:filesystem/types@0.3.0" | "wasi:filesystem/preopens@0.3.0" => {
                    if !grants
                        .filesystem
                        .iter()
                        .any(|grant| grant.provider == import.provider)
                    {
                        return Err(failure(format!(
                            "filesystem provider {} has no Orb grant",
                            import.provider
                        )));
                    }
                }
                "wasi:clocks/system-clock@0.3.0" if import.provider == "system-clock" => {}
                "wasi:sockets/types@0.3.0"
                    if grants
                        .network
                        .iter()
                        .any(|grant| grant.provider == import.provider) => {}
                _ => {
                    return Err(failure(format!(
                        "unsupported host contract {}",
                        import.contract
                    )));
                }
            }
        }
        let mut context = WasiCtxBuilder::new();
        context.allow_tcp(false).allow_udp(false);
        let mut addresses = BTreeSet::new();
        for grant in &grants.network {
            if !self.network.contains(&grant.provider)
                || !imports.iter().any(|import| {
                    import.provider == grant.provider
                        && import.contract == "wasi:sockets/types@0.3.0"
                })
            {
                return Err(failure(
                    "network grants require a configured provider and its WIT import",
                ));
            }
            addresses.extend(grant.connect.iter().copied());
        }
        context.allow_tcp(!addresses.is_empty());
        context.socket_addr_check(move |address, use_| {
            use wasmtime_wasi::sockets::SocketAddrUse;
            let allowed = match use_ {
                SocketAddrUse::TcpConnect => addresses.contains(&address),
                SocketAddrUse::TcpBind => address.ip().is_unspecified() && address.port() == 0,
                _ => false,
            };
            Box::pin(async move { allowed })
        });
        let mut mounts = std::collections::BTreeSet::new();
        for grant in &grants.filesystem {
            if !imports.iter().any(|import| {
                import.provider == grant.provider
                    && import.contract == "wasi:filesystem/preopens@0.3.0"
            }) {
                return Err(failure(format!(
                    "Orb WIT does not import preopens from {}",
                    grant.provider
                )));
            }
            if !grant.mount.starts_with('/')
                || grant.mount == "/"
                || grant
                    .mount
                    .split('/')
                    .skip(1)
                    .any(|part| part.is_empty() || part == "." || part == "..")
                || !mounts.insert(grant.mount.clone())
            {
                return Err(failure(
                    "filesystem mounts must be distinct normalized absolute guest directories",
                ));
            }
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
        Ok(context)
    }
}
