//! One process-wide I/O reactor isolated from Node's libuv pool and from Wasmtime workers.
//!
//! Sockets are nonblocking tasks on a single reactor thread, so an idle stream occupies no
//! thread. Name resolution runs on bounded blocking workers, and admission refuses lookups
//! beyond a bounded queue instead of growing without limit.
use crate::failure;
use std::{future::Future, sync::Arc, sync::OnceLock};
use tembra::Result;
use tokio::sync::Semaphore;

/// Concurrent blocking name resolutions.
const DNS_WORKERS: usize = 4;
/// Admitted standard HTTP name resolutions waiting for a DNS worker.
const DNS_QUEUE: usize = 60;

pub(super) struct Reactor {
    runtime: tokio::runtime::Runtime,
    /// Running and queued standard HTTP name resolutions.
    dns: Arc<Semaphore>,
}

impl Reactor {
    fn new() -> std::io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(DNS_WORKERS)
            .thread_name("tembra-io")
            .enable_io()
            .enable_time()
            .build()?;
        Ok(Self {
            runtime,
            dns: Arc::new(Semaphore::new(DNS_WORKERS + DNS_QUEUE)),
        })
    }

    /// Resolve a host name on a bounded DNS worker, refusing (never queueing without bound)
    /// beyond capacity. `held` lives inside the blocking closure until `getaddrinfo` returns,
    /// so it is released only when the physical lookup ends, even after the caller gave up.
    pub(super) fn resolve<H: Send + 'static>(
        &self,
        host: String,
        port: u16,
        held: H,
    ) -> Result<tokio::task::JoinHandle<std::io::Result<Vec<std::net::SocketAddr>>>> {
        self.dns_job(held, move || {
            std::net::ToSocketAddrs::to_socket_addrs(&(host.as_str(), port)).map(Iterator::collect)
        })
    }

    /// Run one admitted blocking DNS job; `held` drops only after `job` returns.
    pub(super) fn dns_job<H: Send + 'static, T: Send + 'static>(
        &self,
        held: H,
        job: impl FnOnce() -> T + Send + 'static,
    ) -> Result<tokio::task::JoinHandle<T>> {
        let permit = self
            .dns
            .clone()
            .try_acquire_owned()
            .map_err(|_| failure("host DNS capacity is exhausted"))?;
        Ok(self.runtime.spawn_blocking(move || {
            let _held = (permit, held);
            job()
        }))
    }

    /// Run nonblocking socket work and keep its join handle; aborting it drops the work on the
    /// reactor thread, which releases everything the future owns.
    pub(super) fn spawn_joined<F>(&self, task: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.runtime.spawn(task)
    }
}

/// Return the shared reactor, created on first use.
pub(super) fn reactor() -> Result<&'static Reactor> {
    static REACTOR: OnceLock<std::result::Result<Reactor, String>> = OnceLock::new();
    REACTOR
        .get_or_init(|| Reactor::new().map_err(|error| error.to_string()))
        .as_ref()
        .map_err(|error| failure(format!("host I/O reactor is unavailable: {error}")))
}
