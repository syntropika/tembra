//! Embeddings supply time and waits; the native adapter drives the kernel's next deadline.
use futures::{FutureExt, future::LocalBoxFuture};
use std::time::{Duration, Instant};

/// A monotonic local clock used for linger and lifecycle deadlines.
pub trait Clock {
    /// Read whole monotonic milliseconds for lifecycle deadlines.
    fn now_ms(&self) -> u64;
    /// Read fractional milliseconds when the clock provides finer precision.
    fn now(&self) -> f64 {
        self.now_ms() as f64
    }
    /// Wait for a relative duration on the embedding clock.
    fn sleep(&self, milliseconds: u64) -> LocalBoxFuture<'static, ()>;
}

pub(crate) struct SystemClock(Instant);
impl Default for SystemClock {
    fn default() -> Self {
        Self(Instant::now())
    }
}
impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        self.0.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
    }
    fn now(&self) -> f64 {
        self.0.elapsed().as_secs_f64() * 1000.0
    }

    fn sleep(&self, milliseconds: u64) -> LocalBoxFuture<'static, ()> {
        futures_timer::Delay::new(Duration::from_millis(milliseconds)).boxed_local()
    }
}
