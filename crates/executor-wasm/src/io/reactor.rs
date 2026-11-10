//! One process-wide I/O reactor isolated from Node's libuv pool and from Wasmtime workers.
//!
//! Sockets are nonblocking tasks on a single reactor thread, so an idle stream occupies no
//! thread. Standard filesystem work runs inline in Store polls; at most `FILE_WORKERS` polls
//! run at once process-wide, and admission refuses calls beyond a bounded queue instead of
//! growing without limit.
use crate::failure;
use std::{future::Future, sync::Arc, sync::OnceLock};
use tembra::Result;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Concurrent blocking filesystem operations.
const FILE_WORKERS: usize = 4;
/// Hostname resolution must progress independently of blocked filesystem operations.
const DNS_WORKERS: usize = 4;
/// Admitted filesystem operations waiting for a worker.
const FILE_QUEUE: usize = 128;
/// Admitted standard HTTP name resolutions waiting for a DNS worker.
const DNS_QUEUE: usize = 60;

pub(super) struct Reactor {
    runtime: tokio::runtime::Runtime,
    files: Arc<Semaphore>,
    /// Shared active filesystem data work: inline standard WASI Store polls and queued jobs.
    /// Synchronous resource destructors (descriptor close) are deliberately not gated here: they
    /// run on the Orb's own thread inside a guest host call, and waiting for a permit there could
    /// block past the callback deadline or deadlock if a permit holder ever dropped a resource.
    active: Arc<Semaphore>,
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
            files: Arc::new(Semaphore::new(FILE_WORKERS + FILE_QUEUE)),
            active: Arc::new(Semaphore::new(FILE_WORKERS)),
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

    /// Run nonblocking socket work; the task owns its capabilities until it settles.
    pub(super) fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
        drop(self.runtime.spawn(task));
    }

    /// Reserve a bounded filesystem slot before any work is queued.
    pub(super) fn admit_file(&self) -> Result<OwnedSemaphorePermit> {
        self.files
            .clone()
            .try_acquire_owned()
            .map_err(|_| failure("host I/O capacity is exhausted"))
    }

    /// Limit each synchronous poll without retaining a worker while its future is pending.
    pub(super) async fn file_turn<F: Future>(&self, future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let active = self.active.clone();
        let mut acquire = Box::pin(active.clone().acquire_owned());
        std::future::poll_fn(move |context| {
            let _worker = std::task::ready!(acquire.as_mut().poll(context))
                .expect("filesystem worker admission stays open");
            acquire = Box::pin(active.clone().acquire_owned());
            future.as_mut().poll(context)
        })
        .await
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, time::Duration};

    #[test]
    fn concurrent_inline_store_turns_share_one_active_limit() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let reactor = Arc::new(Reactor::new().unwrap());
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let mut threads = Vec::new();
        for _ in 0..12 {
            let (reactor, active, maximum) = (reactor.clone(), active.clone(), maximum.clone());
            threads.push(std::thread::spawn(move || {
                futures::executor::block_on(reactor.file_turn(async {
                    let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                    maximum.fetch_max(count, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(10));
                    active.fetch_sub(1, Ordering::SeqCst);
                }));
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }
        assert!(maximum.load(Ordering::SeqCst) <= FILE_WORKERS);
        assert!(maximum.load(Ordering::SeqCst) > 0);
    }

    #[test]
    fn pending_async_waits_do_not_occupy_filesystem_workers() {
        let reactor = Reactor::new().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            let waiting = async {
                futures::future::join_all((0..FILE_WORKERS).map(|_| {
                    reactor.file_turn(async {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    })
                }))
                .await;
            };
            let probe = async {
                tokio::task::yield_now().await;
                tokio::time::timeout(Duration::from_millis(50), reactor.file_turn(async {}))
                    .await
                    .expect("pending clocks must release their worker budget");
            };
            tokio::join!(waiting, probe);
        });
    }

    #[test]
    fn hostname_resolution_progresses_while_all_filesystem_workers_are_blocked() {
        let reactor = Arc::new(Reactor::new().unwrap());
        let mut holders = Vec::new();
        let (ready, observed) = mpsc::channel();
        let mut release: Vec<mpsc::Sender<()>> = Vec::new();
        for _ in 0..FILE_WORKERS {
            let (send, receive) = mpsc::channel();
            release.push(send);
            let ready = ready.clone();
            let reactor = reactor.clone();
            // Each thread blocks inside one inline Store poll, holding the active budget.
            holders.push(std::thread::spawn(move || {
                futures::executor::block_on(reactor.file_turn(async {
                    ready.send(()).unwrap();
                    let _ = receive.recv();
                }));
            }));
        }
        for _ in 0..FILE_WORKERS {
            observed.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        let (resolved, result) = mpsc::channel();
        reactor.spawn(async move {
            let addresses = tokio::net::lookup_host("localhost:80")
                .await
                .map(|addresses| addresses.collect::<Vec<_>>());
            let _ = resolved.send(addresses);
        });
        let outcome = result.recv_timeout(Duration::from_secs(2));
        // Release every job before reporting a failed assertion or dropping the runtimes.
        drop(release);
        for holder in holders {
            holder.join().unwrap();
        }
        assert!(
            outcome
                .unwrap()
                .unwrap()
                .iter()
                .any(|address| address.ip().is_loopback())
        );
    }
}
