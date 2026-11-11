//! Standard clock waits delegated to upstream WASI with the issuing compiled call's cancellation.
//! Dropping an upstream wait cancels its timer physically. Its unit WIT result cannot carry an
//! error; the enclosing scope remains cancelled and cannot admit further work.
use std::future::Future;
use wasmtime::AsContextMut;
use wasmtime::component::{Accessor, HasData};
use wasmtime_wasi::clocks::{WasiClocks, WasiClocksCtxView, WasiClocksView};
use wasmtime_wasi::p3::bindings::clocks::monotonic_clock::{HostWithStore, Mark};

/// Upstream data view preserves synchronous clock values, resolutions and configured providers.
pub(crate) struct CallClocks;

impl HasData for CallClocks {
    type Data<'a> = WasiClocksCtxView<'a>;
}

/// Capture authority inside the issuing guest task, before its asynchronous wait yields.
fn cancellation<U: crate::sockets::CallCancellation + 'static>(
    store: &Accessor<U, CallClocks>,
) -> Option<crate::http::Cancelled> {
    store.with(|mut access| access.as_context_mut().data_mut().cancellation())
}

async fn wait(
    cancelled: Option<crate::http::Cancelled>,
    upstream: impl Future<Output = wasmtime::Result<()>>,
) -> wasmtime::Result<()> {
    let Some(cancelled) = cancelled else {
        return upstream.await;
    };
    let upstream = std::pin::pin!(upstream);
    match futures::future::select(cancelled, upstream).await {
        // Cancellation wins ties, and dropping the upstream future releases its timer.
        futures::future::Either::Left(((), _)) => Ok(()),
        futures::future::Either::Right((result, _)) => result,
    }
}

impl<U> HostWithStore<U> for CallClocks
where
    U: WasiClocksView + crate::sockets::CallCancellation + 'static,
{
    async fn wait_until(store: &Accessor<U, Self>, when: Mark) -> wasmtime::Result<()> {
        let cancelled = cancellation(store);
        let upstream = store.with_getter::<WasiClocks>(U::clocks);
        wait(
            cancelled,
            <WasiClocks as HostWithStore<U>>::wait_until(&upstream, when),
        )
        .await
    }

    async fn wait_for(store: &Accessor<U, Self>, duration: u64) -> wasmtime::Result<()> {
        let cancelled = cancellation(store);
        let upstream = store.with_getter::<WasiClocks>(U::clocks);
        wait(
            cancelled,
            <WasiClocks as HostWithStore<U>>::wait_for(&upstream, duration),
        )
        .await
    }
}
