//! Native scheduling convenience, kept outside the shared core and browser dependency graph.

use crate::{Host, Result, RuntimeError};
use serde::Serialize;
use std::future::Future;
pub use tembra_native::TokioDriver;

/// Runs one thread-affine Host in an enabled current-thread runtime and awaits its shutdown.
///
/// # Guarantees
/// The callback and Host share one thread and the existing native Driver. Shutdown is awaited
/// whether the callback succeeds or returns an error. A callback error takes precedence over a
/// cleanup error; simultaneous cleanup failure is attached to its details as `cleanup`,
/// with the callback's original details retained as `callbackDetails`. Dropping/panicking out of
/// user work is not a promise of rollback or graceful shutdown.
///
/// # Panics
/// Panics if invoked from within a running Tokio runtime.
///
/// # Errors
/// Returns runtime creation, principal serialization, callback or shutdown errors. It must be
/// called outside an already-running Tokio runtime, as required by Tokio's `block_on`.
pub fn run<T, F, Fut>(name: impl Into<String>, principal: impl Serialize, callback: F) -> Result<T>
where
    F: FnOnce(Host) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| RuntimeError::unavailable("driver", error.to_string()))?;
    let driver = TokioDriver::new();
    let host = Host::with_driver(name, principal, driver.clone())?;
    runtime.block_on(driver.run_until(async move {
        let result = callback(host.clone()).await;
        let cleanup = host.close().await;
        match result {
            Ok(value) => cleanup.map(|()| value),
            Err(mut error) => {
                if let Err(cleanup) = cleanup {
                    let details = serde_json::json!({
                        "callbackDetails": error.details(),
                        "cleanup": cleanup,
                    });
                    error.set_details(details);
                }
                Err(error)
            }
        }
    }))
}
