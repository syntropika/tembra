//! Source-Orb I/O is standard WASI only, dispatched inside the Orb's own Store (see
//! `worker::standard`): filesystem under the shared active filesystem budget, sockets and HTTP
//! (see `crate::http`) on one shared reactor whose bounded resolver tracks every lookup, so a
//! slow peer never occupies the embedding runtime's thread pool.
use std::{collections::BTreeSet, sync::Arc};
use tembra::Result;

mod reactor;
mod scope;

pub(crate) use scope::CallbackScope;

/// Reserve one slot of the process-wide bounded filesystem admission.
pub(crate) fn admit_file() -> Result<tokio::sync::OwnedSemaphorePermit> {
    reactor::reactor()?.admit_file()
}

/// Resolve a host name on the reactor's bounded DNS workers; `held` stays inside the blocking
/// lookup until it physically returns.
pub(crate) fn resolve<H: Send + 'static>(
    host: String,
    port: u16,
    held: H,
) -> Result<tokio::task::JoinHandle<std::io::Result<Vec<std::net::SocketAddr>>>> {
    reactor::reactor()?.resolve(host, port, held)
}

/// Run an arbitrary blocking job through the resolver admission path (tests only).
#[cfg(test)]
pub(crate) fn dns_job<H: Send + 'static, T: Send + 'static>(
    held: H,
    job: impl FnOnce() -> T + Send + 'static,
) -> Result<tokio::task::JoinHandle<T>> {
    reactor::reactor()?.dns_job(held, job)
}

/// Run nonblocking socket work on the reactor thread.
pub(crate) fn spawn_joined<F>(task: F) -> Result<tokio::task::JoinHandle<F::Output>>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    Ok(reactor::reactor()?.spawn_joined(task))
}

/// Bound inline upstream filesystem work without holding a worker across an async wait.
pub(crate) async fn file_turn<F: std::future::Future>(future: F) -> Result<F::Output> {
    Ok(reactor::reactor()?.file_turn(future).await)
}

/// Standard WASI context of one source installation, moved once into its Store.
pub(crate) struct StandardContext {
    pub wasi: wasmtime_wasi::WasiCtxBuilder,
    /// Checked contract imports; generated dispatch refuses any other interface.
    pub allowed: BTreeSet<String>,
    /// The callback epoch that admits and revokes standard WASI calls.
    pub scope: CallbackScope,
    /// HTTP origin grants checked by standard `wasi:http` sends.
    pub grants: Arc<crate::Grants>,
    /// Clocks for the interpreter's own WASIp1 `clock_time_get`, only with the clocks grant.
    pub clocks: Option<crate::worker::wasi::Clocks>,
    /// Whether the provider bound to `wasi:sockets/ip-name-lookup` grants name resolution.
    pub resolve: bool,
}

/// Bound once for an installation; no directory handle or network authority enters the VM.
#[derive(Clone)]
pub(crate) struct SourceIo {
    scope: CallbackScope,
    /// Taken by the Session that owns the Store; never shared between Stores.
    standard: Arc<std::sync::Mutex<Option<StandardContext>>>,
    /// Source built-in authority sent with Load, decided from system grants only.
    builtins: crate::protocol::Builtins,
}

impl SourceIo {
    pub(crate) fn new(
        providers: &crate::providers::Providers,
        imports: &[tembra_contracts::HostImport],
        grants: &crate::Grants,
        cli: &crate::Cli,
    ) -> Result<Self> {
        let scope = CallbackScope::default();
        let prepared = providers.context(imports, grants, cli)?;
        let standard = StandardContext {
            wasi: prepared.wasi,
            clocks: prepared.clocks,
            resolve: prepared.resolve,
            scope: scope.clone(),
            grants: Arc::new(grants.clone()),
            allowed: imports
                .iter()
                .map(|import| import.contract.clone())
                .collect(),
        };
        Ok(Self {
            scope,
            standard: Arc::new(std::sync::Mutex::new(Some(standard))),
            builtins: prepared.builtins,
        })
    }

    /// Source built-in authority for this installation's Load.
    pub(crate) fn builtins(&self) -> crate::protocol::Builtins {
        self.builtins.clone()
    }

    /// Move the standard WASI context into the Store that owns this installation.
    pub(crate) fn take_standard(&self) -> Option<StandardContext> {
        self.standard.lock().expect("standard context").take()
    }

    /// The callback epoch of this Orb's guest callbacks.
    pub(crate) fn scope(&self) -> &CallbackScope {
        &self.scope
    }
}
