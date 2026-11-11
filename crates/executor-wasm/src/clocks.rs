//! Standard `wasi:clocks/monotonic-clock` waits that end with the running call's cancellation.
//!
//! [`CallClocks`] delegates to upstream's clock view. Only `wait-until` and `wait-for` differ:
//! they race the current call's `cancelled` signal, the same signal standard HTTP sends and
//! socket operations observe. The WIT functions have no error lane, so a revoked wait returns
//! early; the guest continues under its cancelled context, where every admitted host operation
//! is refused. The call therefore settles promptly and keeps its revision and descriptors.
use std::time::Duration;
use wasmtime::AsContextMut;
use wasmtime::component::{Accessor, HasData};
use wasmtime_wasi::clocks::WasiClocksCtxView;
use wasmtime_wasi::p3::bindings::clocks::{monotonic_clock, types};

/// Accessor data type of the cancellation-aware clock delegation.
pub(crate) struct CallClocks;

impl HasData for CallClocks {
    type Data<'a> = WasiClocksCtxView<'a>;
}

/// Sleep for `duration` unless the running call is, or becomes, cancelled.
async fn sleep_unless_cancelled<U: crate::sockets::CallCancellation + 'static>(
    store: &Accessor<U, CallClocks>,
    duration: Duration,
) {
    let cancelled = store.with(|mut access| access.as_context_mut().data_mut().cancellation());
    let sleep = std::pin::pin!(tokio::time::sleep(duration));
    match cancelled {
        Some(cancelled) => {
            let _ = futures::future::select(cancelled, sleep).await;
        }
        None => sleep.await,
    }
}

impl<U: crate::sockets::CallCancellation + 'static> monotonic_clock::HostWithStore<U>
    for CallClocks
{
    /// Wait until the monotonic mark, or until the running call is cancelled.
    async fn wait_until(
        store: &Accessor<U, Self>,
        when: monotonic_clock::Mark,
    ) -> wasmtime::Result<()> {
        let now = store.with(|mut view| monotonic_clock::Host::now(&mut view.get()))?;
        if when > now {
            sleep_unless_cancelled(store, Duration::from_nanos(when - now)).await;
        }
        Ok(())
    }

    /// Wait for the duration, or until the running call is cancelled.
    async fn wait_for(
        store: &Accessor<U, Self>,
        duration: types::Duration,
    ) -> wasmtime::Result<()> {
        if duration > 0 {
            sleep_unless_cancelled(store, Duration::from_nanos(duration)).await;
        }
        Ok(())
    }
}
