//! One process-wide I/O reactor isolated from Node's libuv pool and from Wasmtime workers.
//!
//! Sockets are nonblocking tasks on a single reactor thread, so an idle stream occupies no
//! thread. Filesystem operations remain blocking but run on at most `FILE_WORKERS` threads, and
//! admission refuses work beyond a bounded queue instead of growing without limit.
use crate::failure;
use std::{future::Future, sync::Arc, sync::OnceLock};
use tembra::Result;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Concurrent blocking filesystem operations.
const FILE_WORKERS: usize = 4;
/// Admitted filesystem operations waiting for a worker.
const FILE_QUEUE: usize = 128;

pub(super) struct Reactor {
    runtime: tokio::runtime::Runtime,
    files: Arc<Semaphore>,
}

impl Reactor {
    fn new() -> std::io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(FILE_WORKERS)
            .thread_name("tembra-io")
            .enable_io()
            .enable_time()
            .build()?;
        Ok(Self {
            runtime,
            files: Arc::new(Semaphore::new(FILE_WORKERS + FILE_QUEUE)),
        })
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

    /// Run one admitted blocking filesystem job on the bounded worker set.
    pub(super) fn run_file(
        &self,
        permit: OwnedSemaphorePermit,
        job: impl FnOnce() + Send + 'static,
    ) {
        drop(self.runtime.spawn_blocking(move || {
            let _permit = permit;
            job();
        }));
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
