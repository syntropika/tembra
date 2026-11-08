//! Configure providers once; each Orb separately selects mounted directories and access.
use crate::failure;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};
use tembra::Result;
use tembra_contracts::HostImport;
use wasmtime_wasi::{WasiCtxBuilder, filesystem::FsPerms};

pub use tembra_contracts::{FilesystemGrant, Grants, NetworkGrant};

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
        grants.check(
            imports,
            &self.filesystem.keys().cloned().collect(),
            &self.network,
        )?;
        let mut context = WasiCtxBuilder::new();
        context.allow_tcp(false).allow_udp(false);
        let mut addresses = BTreeSet::new();
        for grant in &grants.network {
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
        Ok(context)
    }
}
