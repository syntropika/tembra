//! Standard `wasi:sockets/types` for source and compiled Stores, with call cancellation.
//!
//! [`CallSockets`] delegates every operation to upstream's implementation. Its data view is
//! upstream's own `WasiSocketsCtxView`, so the synchronous methods, the resource drops and the
//! socket address grants are upstream's unchanged. What it adds:
//! - the futures a caller's cancellation can end cooperatively (`tcp connect`, `udp send`,
//!   `udp receive`) race the current call's `cancelled` signal, the same signal standard HTTP
//!   sends observe; a cancelled operation is dropped, which upstream supports, and the guest
//!   gets `error-code.other("call cancelled")`;
//! - `connect` prepares the socket's send writer in the private stream adapter, and `send` hands
//!   upstream that guest-produced stream, so a cancelled send ends physically
//!   (`worker::stream_adapter`);
//! - `receive` and `listen` streams are read by the adapter's pumps and the guest reads them
//!   from a Tembra producer owned by the call that reads it, so the call's cancellation ends a
//!   pending read and closes the upstream producer physically.
//!
//! Upstream constraint: a dropped connect leaves the kernel connection attempt inside the
//! guest's `tcp-socket` resource; it ends when the guest drops the socket or the revision
//! retires.
use std::future::Future;
use wasmtime::AsContextMut;
use wasmtime::component::{Access, Accessor, FutureReader, HasData, Resource, StreamReader};
use wasmtime_wasi::p3::bindings::sockets::types::{
    ErrorCode, HostTcpSocketWithStore, HostUdpSocketWithStore, IpSocketAddress,
};
use wasmtime_wasi::p3::sockets::SocketResult;
use wasmtime_wasi::sockets::{
    TcpSocket, UdpSocket, WasiSockets, WasiSocketsCtxView, WasiSocketsView,
};

/// Accessor data type of the cancellation-aware socket delegation.
pub(crate) struct CallSockets;

impl HasData for CallSockets {
    type Data<'a> = WasiSocketsCtxView<'a>;
}

/// Stores that know which admitted call is running.
pub(crate) trait CallCancellation {
    /// The running call's cancellation, if an admitted call is running.
    fn cancellation(&mut self) -> Option<crate::http::Cancelled>;
}

/// The running call's cancellation, captured synchronously inside the guest call.
fn cancellation<U: CallCancellation + 'static>(
    store: &Accessor<U, CallSockets>,
) -> Option<crate::http::Cancelled> {
    store.with(|mut access| access.as_context_mut().data_mut().cancellation())
}

/// Await `work` unless the call is (or becomes) cancelled; cancellation wins ties, so a
/// cancelled call starts no new socket operation.
async fn unless_cancelled<T>(
    cancelled: Option<crate::http::Cancelled>,
    work: impl Future<Output = SocketResult<T>>,
) -> SocketResult<T> {
    let Some(cancelled) = cancelled else {
        return work.await;
    };
    let work = std::pin::pin!(work);
    match futures::future::select(cancelled, work).await {
        futures::future::Either::Left(((), _)) => {
            Err(ErrorCode::Other(Some("call cancelled".into())).into())
        }
        futures::future::Either::Right((result, _)) => result,
    }
}

impl<U> HostTcpSocketWithStore<U> for CallSockets
where
    U: WasiSocketsView
        + CallCancellation
        + crate::worker::stream_adapter::AdapterStore
        + Send
        + 'static,
{
    /// A connected socket can send: its guest writer is prepared here, while an Accessor exists
    /// (`send` itself is WIT-synchronous and cannot call the adapter).
    async fn connect(
        store: &Accessor<U, Self>,
        socket: Resource<TcpSocket>,
        remote_address: IpSocketAddress,
    ) -> SocketResult<()> {
        let rep = socket.rep();
        let cancelled = cancellation(store);
        let upstream = store.with_getter::<WasiSockets>(U::sockets);
        unless_cancelled(
            cancelled,
            <WasiSockets as HostTcpSocketWithStore<U>>::connect(&upstream, socket, remote_address),
        )
        .await?;
        crate::worker::stream_adapter::prepare(store, rep)
            .await
            .map_err(wasmtime_wasi::p3::sockets::SocketError::trap)
    }

    async fn listen(
        mut store: Access<'_, U, Self>,
        socket: Resource<TcpSocket>,
    ) -> SocketResult<StreamReader<Resource<TcpSocket>>> {
        let upstream = Access::<U, WasiSockets>::new(store.as_context_mut(), U::sockets);
        let accepts = <WasiSockets as HostTcpSocketWithStore<U>>::listen(upstream, socket).await?;
        crate::worker::stream_adapter::listen(store.as_context_mut(), accepts)
            .map_err(wasmtime_wasi::p3::sockets::SocketError::trap)
    }

    /// Upstream's real `send` consumes the socket's prepared guest-produced stream, fed from
    /// `data` by a pump the current command's cancellation aborts (`stream_adapter::send`).
    fn send(
        mut store: Access<'_, U, Self>,
        socket: Resource<TcpSocket>,
        data: StreamReader<u8>,
    ) -> wasmtime::Result<FutureReader<Result<(), ErrorCode>>> {
        crate::worker::stream_adapter::send(store.as_context_mut(), socket, data)
    }

    /// Upstream's byte stream is read by the stream adapter's pump; the guest reads the same
    /// bytes from a stream its call's cancellation can end. The completion future (and with it
    /// every receive error) is upstream's own.
    fn receive(
        mut store: Access<'_, U, Self>,
        socket: Resource<TcpSocket>,
    ) -> wasmtime::Result<(StreamReader<u8>, FutureReader<Result<(), ErrorCode>>)> {
        let upstream = Access::<U, WasiSockets>::new(store.as_context_mut(), U::sockets);
        let (data, done) = <WasiSockets as HostTcpSocketWithStore<U>>::receive(upstream, socket)?;
        crate::worker::stream_adapter::receive(store.as_context_mut(), data, done)
    }
}

impl<U> HostUdpSocketWithStore<U> for CallSockets
where
    U: WasiSocketsView + CallCancellation + Send + 'static,
{
    async fn send(
        store: &Accessor<U, Self>,
        socket: Resource<UdpSocket>,
        data: Vec<u8>,
        remote_address: Option<IpSocketAddress>,
    ) -> SocketResult<()> {
        let cancelled = cancellation(store);
        let upstream = store.with_getter::<WasiSockets>(U::sockets);
        unless_cancelled(
            cancelled,
            <WasiSockets as HostUdpSocketWithStore<U>>::send(
                &upstream,
                socket,
                data,
                remote_address,
            ),
        )
        .await
    }

    async fn receive(
        store: &Accessor<U, Self>,
        socket: Resource<UdpSocket>,
    ) -> SocketResult<(Vec<u8>, IpSocketAddress)> {
        let cancelled = cancellation(store);
        let upstream = store.with_getter::<WasiSockets>(U::sockets);
        unless_cancelled(
            cancelled,
            <WasiSockets as HostUdpSocketWithStore<U>>::receive(&upstream, socket),
        )
        .await
    }
}
