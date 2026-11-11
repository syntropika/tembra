//! Shared I/O of component Stores: sockets and HTTP (see `crate::http`) run on one shared reactor
//! whose bounded resolver tracks every lookup, so a slow peer never occupies the embedding
//! runtime's thread pool. Each revision's callback epochs live in [`CallbackScope`].
use tembra::Result;

mod reactor;
mod scope;

pub(crate) use scope::CallbackScope;

/// Resolve a host name on the reactor's bounded DNS workers; `held` stays inside the blocking
/// lookup until it physically returns.
pub(crate) fn resolve<H: Send + 'static>(
    host: String,
    port: u16,
    held: H,
) -> Result<tokio::task::JoinHandle<std::io::Result<Vec<std::net::SocketAddr>>>> {
    reactor::reactor()?.resolve(host, port, held)
}

/// Run nonblocking socket work on the reactor thread.
pub(crate) fn spawn_joined<F>(task: F) -> Result<tokio::task::JoinHandle<F::Output>>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    Ok(reactor::reactor()?.spawn_joined(task))
}
