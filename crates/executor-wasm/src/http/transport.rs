//! Physical transport for standard `wasi:http` sends (resolver, connect, TLS, HTTP/1).
//!
//! The sender follows upstream `wasmtime-wasi-http` 51.0.0-rc.1 `default_send_request`
//! (Apache-2.0 WITH LLVM-exception); diff against it on upgrades. It differs only where Tembra
//! needs authority over physical work: the canonical [`Target`], a bounded tracked resolver,
//! abortable reactor connects, and ledgers.
//!
//! Name resolution runs on the shared reactor's bounded DNS workers and connect/TLS/handshake
//! run as reactor tasks, so a blocked filesystem worker or a slow resolver never stalls a Store.
//! Every unit of physical work holds a [`Held`] guard released only when the work itself is
//! gone: inside the blocking lookup closure until `getaddrinfo` returns, inside the reactor
//! connect future until the reactor drops it, and inside the connection driver until it closes.
//!
//! Ownership of a response body (one rule for source and compiled Stores):
//! - never read: revision-owned; it survives commands (callbacks or compiled calls);
//! - first read by the host during a command: owned by that command; when the command ends, or
//!   its caller cancels it, an unfinished body is closed and the command waits for the close;
//! - closed by the guest (`err` on the `res` future given to `consume-body`): closed at once.
//!
//! The Store is driven only inside commands and commands are serialized per Session, so "the
//! command during which the host first read the body" is always exactly one command.
use super::{Host, Target};
use bytes::Bytes;
use std::{
    future::{Future, poll_fn},
    net::SocketAddr,
    pin::{Pin, pin},
    sync::{Arc, Mutex, Weak},
    task::{Context, Poll, Waker, ready},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
    sync::watch,
};
use wasmtime_wasi_http::{Error, RequestOptions, WasiBody, io::TokioIo};

/// Upstream's default when a request sets no timeout.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

type Shared<T> = Arc<Mutex<T>>;

/// Live physical work units; a [`Held`] drop is the physical acknowledgement.
#[derive(Clone)]
pub(crate) struct Ledger(Arc<watch::Sender<usize>>);

impl Default for Ledger {
    fn default() -> Self {
        Self(Arc::new(watch::Sender::new(0)))
    }
}

impl Ledger {
    /// Count one unit of physical work until the guard drops.
    pub(crate) fn hold(&self) -> Held {
        self.0.send_modify(|count| *count += 1);
        Held(vec![Arc::clone(&self.0)])
    }

    /// Physical work units still alive.
    #[cfg(test)]
    pub(crate) fn live(&self) -> usize {
        *self.0.borrow()
    }

    /// Wait until every unit has physically ended.
    pub(crate) async fn settled(&self) {
        let mut live = self.0.subscribe();
        let _ = live.wait_for(|count| *count == 0).await;
    }
}

/// One counted unit of physical work, possibly counted by several ledgers.
pub(crate) struct Held(Vec<Arc<watch::Sender<usize>>>);

impl Held {
    /// Also count this unit in `ledger`, if any.
    fn and(mut self, ledger: Option<&Ledger>) -> Self {
        if let Some(ledger) = ledger {
            self.0.extend(std::mem::take(&mut ledger.hold().0));
        }
        self
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        for ledger in &self.0 {
            ledger.send_modify(|count| *count -= 1);
        }
    }
}

/// The ledgers that own a request's resolver and connect work: the Store's work ledger (its
/// teardown waits for it) and, for a compiled call, the sending call's ledger.
#[derive(Clone)]
struct Owners {
    store_work: Ledger,
    sending_call: Option<Ledger>,
}

impl Owners {
    fn hold(&self) -> Held {
        self.store_work.hold().and(self.sending_call.as_ref())
    }
}

/// The guest's response-handling result (upstream's `send_request` third argument, resolved
/// from the `res` future given to `consume-body` when it resolves): `Err` closes the response.
pub(crate) type Handling = Pin<Box<dyn Future<Output = Result<(), Error>> + Send>>;

/// What one send needs beyond the request: its owners and the guest's handling result.
pub(crate) struct Exchange {
    /// The sending compiled call's work ledger (`None` for source callbacks, whose send tasks
    /// never outlive them and are counted by the Store's work ledger).
    pub(crate) sending_call: Option<Ledger>,
    pub(crate) handling: Handling,
}

/// The Store's command boundary for body ownership. Commands (source callbacks, compiled calls)
/// are serialized per Session and the Store is only driven inside one, so there is at most one
/// current command.
#[derive(Clone)]
pub(crate) struct Commands(Shared<Current>, Ledger);

#[derive(Default)]
struct Current {
    /// The current compiled call's work ledger (`None`: only the Store's work ledger).
    call: Option<Ledger>,
    /// Bodies first read during the current command.
    owned: Vec<Weak<Mutex<Link>>>,
    /// The caller cancelled the current command. A body whose first read comes after the
    /// cancellation (the response head arrived, the guest had not read yet) is closed when it
    /// registers, so no window exists between head and first read.
    cancelled: bool,
}

impl Commands {
    /// A compiled call starts: work it starts later is charged to `call` too.
    pub(crate) fn begin(&self, call: Option<Ledger>) {
        let mut current = self.0.lock().expect("commands");
        current.call = call;
        current.owned.clear();
        current.cancelled = false;
    }

    /// The caller cancelled the current command: close its unfinished bodies now. The hold is
    /// taken here, synchronously, and released by each driver after its socket closed.
    pub(crate) fn cancel(&self) {
        let (owned, call) = {
            let mut current = self.0.lock().expect("commands");
            current.cancelled = true;
            (current.owned.clone(), current.call.clone())
        };
        self.close(owned, call.as_ref());
    }

    /// The current command ends (normally or cancelled): close its unfinished bodies, charged
    /// to it, and start the next command's ownership.
    pub(crate) fn end(&self) {
        let (owned, call) = {
            let mut current = self.0.lock().expect("commands");
            current.cancelled = false;
            (std::mem::take(&mut current.owned), current.call.take())
        };
        self.close(owned, call.as_ref());
    }

    fn close(&self, owned: Vec<Weak<Mutex<Link>>>, call: Option<&Ledger>) {
        for link in owned.iter().filter_map(Weak::upgrade) {
            let mut link = link.lock().expect("link");
            if link.driver && !link.finished && link.close.is_none() {
                link.close = Some(self.1.hold().and(call));
                if let Some(driver) = link.waker.take() {
                    driver.wake();
                }
            }
        }
    }

    /// Hold one unit for the Store and the current command.
    fn hold_current(&self) -> Held {
        let call = self.0.lock().expect("commands").call.clone();
        self.1.hold().and(call.as_ref())
    }

    /// The body behind `link` was first read now: the current command owns it.
    fn own(&self, link: &Shared<Link>) {
        let cancelled = {
            let mut current = self.0.lock().expect("commands");
            current.owned.push(Arc::downgrade(link));
            current.cancelled.then(|| current.call.clone())
        };
        // First read after the command was cancelled: close it now, charged to that command.
        if let Some(call) = cancelled {
            self.close(vec![Arc::downgrade(link)], call.as_ref());
        }
    }
}

/// Transport state of one Store.
#[derive(Clone)]
pub(crate) struct Transport {
    /// Active physical work: resolver and connect jobs, response teardown, and closes in
    /// progress. Callback retirement and revision teardown wait for it; idle drivers are not in
    /// it, so no command ever waits for a response someone merely retains.
    pub(crate) store_work: Ledger,
    /// Connection drivers, including idle drivers of retained responses; revision resources.
    pub(crate) drivers: Ledger,
    /// Command boundary for body ownership.
    pub(crate) commands: Commands,
    roots: Arc<rustls::RootCertStore>,
}

impl Default for Transport {
    fn default() -> Self {
        let store_work = Ledger::default();
        Self {
            commands: Commands(Arc::default(), store_work.clone()),
            store_work,
            drivers: Ledger::default(),
            roots: Arc::new(rustls::RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.into(),
            }),
        }
    }
}

trait Stream: AsyncRead + AsyncWrite + Send + Unpin + 'static {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> Stream for T {}

type Sender = hyper::client::conn::http1::SendRequest<WasiBody>;
type Connection = hyper::client::conn::http1::Connection<TokioIo<Box<dyn Stream>>, WasiBody>;

/// Abort a reactor task when its caller stops waiting; the task's own guard acknowledges.
struct Joined<T>(tokio::task::JoinHandle<T>);

impl<T> Future for Joined<T> {
    type Output = Result<T, Error>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0)
            .poll(context)
            .map_err(|error| Error::InternalError(Some(format!("host transport task: {error}"))))
    }
}

impl<T> Drop for Joined<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Transport {
    /// Trust exactly `roots` instead of the public web roots (tests only).
    #[cfg(test)]
    pub(crate) fn trusting(roots: rustls::RootCertStore) -> Self {
        Self {
            roots: Arc::new(roots),
            ..Self::default()
        }
    }

    /// Send one admitted request, keeping upstream request-option timeouts. Returns the
    /// response and the connection driver whose drop is the physical acknowledgement.
    pub(crate) async fn send(
        &self,
        target: Target,
        mut request: http::Request<WasiBody>,
        options: Option<RequestOptions>,
        exchange: Exchange,
    ) -> Result<(http::Response<WasiBody>, Driver), Error> {
        let owners = Owners {
            store_work: self.store_work.clone(),
            sending_call: exchange.sending_call,
        };
        let Target {
            tls,
            ref host,
            port,
            ..
        } = target;
        let timeout = |pick: fn(&RequestOptions) -> Option<Duration>| {
            options.as_ref().and_then(pick).unwrap_or(DEFAULT_TIMEOUT)
        };
        let connect_timeout = timeout(|options| options.connect_timeout);
        let first_byte_timeout = timeout(|options| options.first_byte_timeout);
        let between_bytes_timeout = timeout(|options| options.between_bytes_timeout);

        // `connect_timeout` covers resolution, TCP connect, TLS and the HTTP handshake.
        let connect = async {
            let addresses = match host {
                Host::Ip(ip) => vec![SocketAddr::new(*ip, port)],
                Host::Name(host) => {
                    let lookup = crate::io::resolve(host.clone(), port, owners.hold())
                        .map_err(|_| Error::ConnectionLimitReached)?;
                    // Dropping the join handle leaves the lookup (and its guard) running.
                    lookup
                        .await
                        .map_err(|_| Error::DnsTimeout)?
                        .map_err(|error| Error::DnsError {
                            rcode: Some(error.to_string()),
                            info_code: None,
                        })?
                }
            };
            if addresses.is_empty() {
                return Err(Error::DestinationNotFound);
            }
            let job = owners.hold();
            let roots = tls.then(|| Arc::clone(&self.roots));
            let name = host.clone();
            let task = crate::io::spawn_joined(async move {
                let _job = job;
                establish(addresses, roots, name).await
            })
            .map_err(|_| Error::ConnectionLimitReached)?;
            Joined(task).await?
        };
        let (mut sender, connection) = tokio::time::timeout(connect_timeout, connect)
            .await
            .map_err(|_| Error::ConnectionTimeout)??;
        let driver = self.drivers.hold();
        let link: Shared<Link> = Arc::new(Mutex::new(Link {
            driver: true,
            ..Link::default()
        }));
        let body_link = Arc::clone(&link);
        let commands = self.commands.clone();

        // Origin-form request target and `Host` come from the same canonical target that was
        // granted and connected; scheme and authority are only sent to proxies.
        *request.uri_mut() = http::Uri::from(target.path.clone());
        request.headers_mut().insert(
            http::header::HOST,
            http::HeaderValue::from_str(&target.authority())
                .map_err(|_| Error::HttpRequestUriInvalid)?,
        );
        let send = async move {
            let response = tokio::time::timeout(first_byte_timeout, sender.send_request(request))
                .await
                .map_err(|_| Error::ConnectionReadTimeout)?
                .map_err(Error::from)?;
            // Tokio refuses a zero interval; 1ns is effectively zero.
            let between = between_bytes_timeout.max(Duration::from_nanos(1));
            let mut timeout = tokio::time::interval(between);
            timeout.reset();
            Ok::<_, Error>(response.map(|incoming| Incoming {
                incoming,
                timeout,
                link: body_link,
                commands,
            }))
        };
        let mut send = pin!(send);
        let mut connection = Some(connection);
        // Drive connection I/O while waiting for the response head.
        let response = poll_fn(|context| match send.as_mut().poll(context) {
            Poll::Ready(result) => Poll::Ready(result),
            Poll::Pending => {
                let Some(io) = connection.as_mut() else {
                    return Poll::Pending;
                };
                let result = ready!(Pin::new(io).poll(context));
                connection = None;
                match result {
                    Ok(()) => send.as_mut().poll(context),
                    Err(error) => Poll::Ready(Err(Error::from(error))),
                }
            }
        })
        .await?;
        Ok((
            response.map(http_body_util::BodyExt::boxed_unsync),
            Driver {
                connection,
                handling: Some(exchange.handling),
                armed: Arc::default(),
                link,
                commands: self.commands.clone(),
                _driver: driver,
            },
        ))
    }
}

/// Connect, optionally negotiate TLS, and complete the HTTP/1 handshake on the reactor.
async fn establish(
    addresses: Vec<SocketAddr>,
    roots: Option<Arc<rustls::RootCertStore>>,
    host: Host,
) -> Result<(Sender, Connection), Error> {
    let stream = TcpStream::connect(&addresses[..])
        .await
        .map_err(Error::Connect)?;
    let stream: Box<dyn Stream> = match roots {
        Some(roots) => {
            let config = rustls::ClientConfig::builder()
                .with_root_certificates(Arc::clone(&roots))
                .with_no_client_auth();
            let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
            // SNI and verification use the request host, an IP address or a DNS name.
            let name = match host {
                Host::Ip(ip) => rustls::pki_types::ServerName::from(ip),
                // Unreachable for a canonical target (LDH names only); refused, never guessed.
                Host::Name(name) => rustls::pki_types::ServerName::try_from(name)
                    .map_err(|_| Error::HttpRequestUriInvalid)?,
            };
            Box::new(connector.connect(name, stream).await.map_err(Error::Tls)?)
        }
        None => Box::new(stream),
    };
    hyper::client::conn::http1::Builder::new()
        .handshake(TokioIo::new(stream))
        .await
        .map_err(Error::from)
}

/// State shared by a response body and its connection driver.
#[derive(Default)]
struct Link {
    /// The driver has not dropped yet.
    driver: bool,
    /// The body reached its end; there is nothing left to close.
    finished: bool,
    /// The body was read at least once (it has an owning command).
    owned: bool,
    /// Held from the body's drop until the driver's drop (teardown in progress).
    teardown: Option<Held>,
    /// Held from a close request (owning command ended or was cancelled) until the socket closed.
    close: Option<Held>,
    /// The driver task's waker, so a close request runs it.
    waker: Option<Waker>,
}

/// Response body with upstream's between-bytes timeout; its first read assigns its owner.
struct Incoming {
    incoming: hyper::body::Incoming,
    timeout: tokio::time::Interval,
    link: Shared<Link>,
    commands: Commands,
}

impl Drop for Incoming {
    fn drop(&mut self) {
        let mut link = self.link.lock().expect("link");
        if link.driver && !link.finished {
            // Charged to the command that drops the body, which may be a later one.
            link.teardown = Some(self.commands.hold_current());
        }
    }
}

impl http_body::Body for Incoming {
    type Data = Bytes;
    type Error = Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Bytes>, Error>>> {
        let first = !std::mem::replace(&mut self.link.lock().expect("link").owned, true);
        if first {
            self.commands.own(&self.link);
        }
        let frame = match Pin::new(&mut self.incoming).poll_frame(context) {
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Ready(Some(Err(error))) => Poll::Ready(Some(Err(if error.is_timeout() {
                Error::HttpResponseTimeout
            } else {
                Error::from(error)
            }))),
            Poll::Ready(Some(Ok(frame))) => {
                self.timeout.reset();
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Pending => {
                ready!(self.timeout.poll_tick(context));
                Poll::Ready(Some(Err(Error::ConnectionReadTimeout)))
            }
        };
        if matches!(frame, Poll::Ready(None)) || self.incoming.is_end_stream() {
            self.link.lock().expect("link").finished = true;
        }
        frame
    }

    fn is_end_stream(&self) -> bool {
        self.incoming.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.incoming.size_hint()
    }
}

/// The connection driver handed to upstream. It owns the socket; its drop closes it and is the
/// physical acknowledgement counted by the revision's drivers ledger.
pub(crate) struct Driver {
    connection: Option<Connection>,
    /// The guest's response-handling result; `Err` closes the connection now, even while the
    /// body is idle (no producer or consumer poll is needed).
    handling: Option<Handling>,
    /// Taken when the handling result is delivered (inside the Store poll that resolved it) and
    /// released once this driver has processed it.
    armed: Shared<Option<Held>>,
    link: Shared<Link>,
    commands: Commands,
    _driver: Held,
}

/// Wakes the driver for its handling result and counts the pending processing as work of the
/// current command before the driver task is even scheduled.
///
/// Take-before-release relies on the Store's `current_thread` runtime: this wake (inside a
/// Store poll) and the driver's poll (a task on the same runtime) never run concurrently, so
/// the driver cannot release a hold between this take and its own processing.
struct Arming {
    task: Waker,
    armed: Shared<Option<Held>>,
    commands: Commands,
}

impl std::task::Wake for Arming {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        {
            let mut armed = self.armed.lock().expect("armed");
            if armed.is_none() {
                *armed = Some(self.commands.hold_current());
            }
        }
        self.task.wake_by_ref();
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        // Close the socket first; only then is teardown or a close acknowledged.
        drop(self.connection.take());
        let mut link = self.link.lock().expect("link");
        link.driver = false;
        link.teardown = None;
        link.close = None;
    }
}

impl Driver {
    /// Close the connection now and fail the exchange: a cut request or response is never
    /// reported as a clean transmission.
    fn close(&mut self, error: Error) -> Poll<Result<(), Error>> {
        drop(self.connection.take());
        self.armed.lock().expect("armed").take();
        self.link.lock().expect("link").close = None;
        Poll::Ready(Err(error))
    }
}

impl Future for Driver {
    type Output = Result<(), Error>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        if let Some(handling) = this.handling.as_mut() {
            let waker = Waker::from(Arc::new(Arming {
                task: context.waker().clone(),
                armed: Arc::clone(&this.armed),
                commands: this.commands.clone(),
            }));
            match handling.as_mut().poll(&mut Context::from_waker(&waker)) {
                // The guest closed the response.
                Poll::Ready(Err(error)) => return this.close(error),
                // Handled: the connection keeps running to its own end.
                Poll::Ready(Ok(())) => this.handling = None,
                Poll::Pending => {}
            }
        }
        {
            let mut link = this.link.lock().expect("link");
            if link.close.is_some() {
                drop(link);
                return this.close(Error::InternalError(Some(
                    "the command reading this response ended".into(),
                )));
            }
            link.waker = Some(context.waker().clone());
        }
        // Any delivered handling result has been processed.
        this.armed.lock().expect("armed").take();
        let Some(connection) = this.connection.as_mut() else {
            return Poll::Ready(Ok(()));
        };
        let result = ready!(Pin::new(connection).poll(context));
        this.connection = None;
        Poll::Ready(result.map_err(|error| {
            if error.is_timeout() {
                Error::HttpResponseTimeout
            } else {
                Error::from(error)
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Later work (a body first read, dropped, or closed in a later command) is charged to the
    /// command current at that moment, never to the finished sending call, and a command's end
    /// takes the close hold synchronously.
    #[test]
    fn later_work_is_charged_to_the_current_command() {
        let transport = Transport::default();
        let (sender, closer) = (Ledger::default(), Ledger::default());
        let owners = Owners {
            store_work: transport.store_work.clone(),
            sending_call: Some(sender.clone()),
        };
        // The send's own work (resolver, connect) belongs to the sending call.
        let connect = owners.hold();
        assert_eq!((transport.store_work.live(), sender.live()), (1, 1));
        drop(connect);

        // A later call reads the body first: it owns it; its end closes it, charged to it.
        transport.commands.begin(Some(closer.clone()));
        let link: Shared<Link> = Arc::new(Mutex::new(Link {
            driver: true,
            owned: true,
            ..Link::default()
        }));
        transport.commands.own(&link);
        transport.commands.end();
        assert_eq!(
            (transport.store_work.live(), sender.live(), closer.live()),
            (1, 0, 1)
        );
        // The driver closes the socket, then releases the hold.
        link.lock().unwrap().close = None;
        assert_eq!((transport.store_work.live(), closer.live()), (0, 0));

        // A body first read after its command was cancelled is closed when it registers
        // (the window between the response head and the first read).
        transport.commands.begin(Some(closer.clone()));
        transport.commands.cancel();
        let late: Shared<Link> = Arc::new(Mutex::new(Link {
            driver: true,
            owned: true,
            ..Link::default()
        }));
        transport.commands.own(&late);
        assert!(late.lock().unwrap().close.is_some());
        assert_eq!(closer.live(), 1);
        // The driver closes the socket and drops (as `Driver::drop` does).
        {
            let mut late = late.lock().unwrap();
            late.close = None;
            late.driver = false;
        }
        transport.commands.end();
        assert_eq!(closer.live(), 0);
        // The next command starts uncancelled.
        transport.commands.begin(Some(closer.clone()));
        let fresh: Shared<Link> = Arc::new(Mutex::new(Link {
            driver: true,
            owned: true,
            ..Link::default()
        }));
        transport.commands.own(&fresh);
        assert!(fresh.lock().unwrap().close.is_none());
        transport.commands.end();
        fresh.lock().unwrap().close = None;

        // A finished body is never closed; an idle unread body has no owner at all.
        transport.commands.begin(Some(closer.clone()));
        let finished: Shared<Link> = Arc::new(Mutex::new(Link {
            driver: true,
            owned: true,
            finished: true,
            ..Link::default()
        }));
        transport.commands.own(&finished);
        transport.commands.end();
        assert_eq!(closer.live(), 0);

        // Handling delivery (Arming) and teardown are charged to the current command.
        transport.commands.begin(Some(closer.clone()));
        let armed: Shared<Option<Held>> = Arc::default();
        let arming = Arc::new(Arming {
            task: Waker::noop().clone(),
            armed: Arc::clone(&armed),
            commands: transport.commands.clone(),
        });
        std::task::Wake::wake_by_ref(&arming);
        std::task::Wake::wake_by_ref(&arming);
        assert_eq!((sender.live(), closer.live()), (0, 1));
        armed.lock().unwrap().take();
        transport.commands.end();
        let held = transport.commands.hold_current();
        assert_eq!((transport.store_work.live(), closer.live()), (1, 0));
        drop(held);
    }
}
