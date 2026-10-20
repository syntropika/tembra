use std::{future::Future, rc::Rc, time::Duration};

use futures::future::LocalBoxFuture;
use tembra_core::{Driver, Result, RuntimeError};
use tokio::{task::LocalSet, time::Instant};

/// A local scheduler that keeps non-Send Host tasks on the embedding thread.
///
/// The embedding owns a current-thread Tokio runtime with I/O and timers enabled,
/// and drives this driver's local set until managed Host shutdown completes.
/// Dropping the executor early drops its tasks; it is not graceful Host shutdown.
pub struct TokioDriver {
    local: LocalSet,
    origin: Instant,
}

impl TokioDriver {
    /// Creates a local set without requiring an entered Tokio runtime.
    pub fn new() -> Rc<Self> {
        Rc::new(Self {
            local: LocalSet::new(),
            origin: Instant::now(),
        })
    }

    /// Drives the local set alongside `future` inside an enabled Tokio runtime.
    ///
    /// Spawned work may outlive this future. Await Host shutdown before dropping
    /// the driver. As with LocalSet, nested drives of the same set are invalid.
    pub async fn run_until<F: Future>(&self, future: F) -> F::Output {
        self.local.run_until(future).await
    }
}

impl Driver for TokioDriver {
    fn spawn(&self, future: LocalBoxFuture<'static, ()>) {
        drop(self.local.spawn_local(future));
    }

    fn sleep(&self, duration: Duration) -> LocalBoxFuture<'static, ()> {
        // Constructing Sleep eagerly would panic outside an entered timer runtime.
        Box::pin(async move { tokio::time::sleep(duration).await })
    }

    fn now(&self) -> Duration {
        self.origin.elapsed()
    }
}

/// Generates a fresh 128-bit lowercase hexadecimal Host incarnation using the OS.
///
/// # Errors
/// Returns unavailable/entropy if the operating-system random source fails.
pub fn random_incarnation() -> Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| RuntimeError::unavailable("entropy", error.to_string()))?;
    use std::fmt::Write;
    let mut encoded = String::with_capacity(32);
    for byte in bytes {
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    Ok(encoded)
}
