use std::{rc::Rc, time::Duration};

use futures::{FutureExt, future::LocalBoxFuture};
use gloo_timers::future::TimeoutFuture;
use tembra_core::{Driver, RuntimeError};
use wasm_bindgen::{JsCast, JsValue};
use web_sys::Performance;

/// Local browser scheduling with monotonic, drop-cancelled timers.
pub struct BrowserDriver {
    clock: Performance,
    origin: f64,
}

impl BrowserDriver {
    /// Uses the current Window or Worker environment's monotonic performance clock.
    ///
    /// # Errors
    /// Refuses environments without a native Performance clock.
    pub fn new() -> Result<Rc<Self>, RuntimeError> {
        let clock = js_sys::Reflect::get(&js_sys::global(), &JsValue::from_str("performance"))
            .ok()
            .and_then(|value| value.dyn_into::<Performance>().ok())
            .ok_or_else(|| RuntimeError::unavailable("browser_clock", "no performance clock"))?;
        Ok(Rc::new(Self {
            origin: clock.now(),
            clock,
        }))
    }
}

impl Driver for BrowserDriver {
    fn spawn(&self, future: LocalBoxFuture<'static, ()>) {
        wasm_bindgen_futures::spawn_local(future);
    }

    fn sleep(&self, duration: Duration) -> LocalBoxFuture<'static, ()> {
        let clock = self.clock.clone();
        let deadline = clock.now() + duration.as_secs_f64() * 1_000.0;
        async move {
            loop {
                let remaining = deadline - clock.now();
                if remaining <= 0.0 {
                    return;
                }
                // Browser timeout delays are signed 32-bit milliseconds. Recheck long
                // deadlines after each chunk; dropping this future cancels its live timer.
                let millis = remaining.ceil().clamp(1.0, f64::from(i32::MAX)) as u32;
                TimeoutFuture::new(millis).await;
            }
        }
        .boxed_local()
    }

    fn now(&self) -> Duration {
        Duration::from_secs_f64((self.clock.now() - self.origin).max(0.0) / 1_000.0)
    }
}
