//! Serve incoming HTTP through a compiled Orb's exact `wasi:http/handler@0.3.0` export.
//!
//! The application owns the listener, authentication and routing. It passes each accepted
//! request to [`serve`] with the [`Reference`] it selected and the principal
//! it authenticated. `serve` runs the request under its own root, a task of the host's executor
//! that lasts until the call has physically ended, so the guest keeps its authority (dependency
//! calls, granted outgoing requests) while the body streams after the head was handed out. The
//! root ends exactly when the call does, after a complete body, a disconnect, a cancellation, an
//! error or a timeout. The call is an ordinary kernel call of a host-local method: the kernel
//! selects the current revision, admits it, pins it until the call has physically ended, and
//! drains it on reload or close. The handler's owned `request` and
//! `response` resources stay in the Orb's own Store; they never become portable Orb values, and
//! no other Orb or foreign host can call the handler.
//!
//! # Lifetime of one request
//!
//! - **Head.** [`serve`] returns as soon as the guest returned its response: the status and
//!   headers, a [`ResponseBody`] that streams what the guest writes, and [`Settled`], which
//!   resolves when the call has physically ended. Publishing the head never ends the call or
//!   releases its kernel pin.
//! - **Body.** The guest's body producer keeps running in the same admitted call. The Store
//!   drives it while the application polls [`ResponseBody`] on any thread; one buffered chunk
//!   gives backpressure. The call ends only after the guest's task group finished (every task
//!   and subtask of the call exited) **and** the response body was delivered to its end,
//!   dropped, or aborted.
//! - **Disconnect.** Dropping [`ResponseBody`] before its end (the client went away) resolves
//!   the guest's `response.new` transmission future with `connection-terminated` and closes the
//!   request body with `connection-terminated`. A writing guest also sees its next write
//!   dropped. A guest that ignores all three keeps the call (and the revision's one callback
//!   slot) until the engine's `timeout_ms`.
//! - **Cancellation.** [`Settled::cancel`] cancels the request's root, which ends the call's
//!   callback epoch: the request body is closed, the response body is taken away from the
//!   application (its next poll fails), and the transmission future fails, then the call
//!   settles as usual.
//! - **Timeout and traps.** The engine's `timeout_ms` bounds the whole call, including
//!   streaming. A timeout or a trap is terminal, as for every compiled call: the Store is
//!   quarantined and the revision serves no further call. The response body then ends with an
//!   error, never with a clean end, even if the guest had already resolved its trailers.
//! - **Reload and close.** New calls reach the new revision. A streaming call keeps its old
//!   revision until it settles; `Host::close` waits for it.
//! - **Errors.** A guest `error-code`, a trap, or a refusal before a head is reported by
//!   [`serve`] only after the call settled, so no physical work outlives the returned error. A
//!   guest that returns an `error-code` but leaves a task running in its call delays that error
//!   until the task ends, at most `timeout_ms`. With no response to deliver, the request body is
//!   closed at once, so a task still reading it ends.
//! - **One request per revision.** A revision serves one callback at a time, so a long stream
//!   holds its revision: other calls wait for admission and are refused after half of
//!   `timeout_ms`.
//!
//! [`Settled`] only observes completion: the host's executor drives the call, and the root is
//! not tied to it. Dropping it stops nothing and does not end the root.
use bytes::Bytes;
use futures::{
    FutureExt,
    future::{Either, LocalBoxFuture},
    task::AtomicWaker,
};
use http_body::{Body, Frame};
use http_body_util::{BodyExt, combinators::UnsyncBoxBody};
use std::{
    pin::Pin,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context as TaskContext, Poll},
};
use tembra::{Context, Host, Reference, Result};
use tokio::sync::{oneshot, watch};

mod carrier;
mod refusal;
pub(crate) use carrier::begin as begin_carrier;
pub use carrier::{
    IngressEvent, IngressField, IngressPort, IngressRequestChunk, IngressRequestHead,
    IngressResponseFrame, IngressResponseHead,
};
pub(crate) use wasmtime_wasi_http::Error as HttpError;

/// The handler's only method.
pub(crate) const HANDLE: &str = "handle";

/// Name of the host-local service through which an Orb's incoming HTTP handler is called.
///
/// Acquire a reference to it like any service (`host.get(&handler_service("web"), ...)`); only
/// [`serve`] can call it.
pub fn handler_service(orb: &str) -> String {
    tembra_contracts::service_name(orb, tembra_contracts::HTTP_HANDLER)
}

/// How the response body's delivery ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Delivery {
    /// The application read the body to its end.
    Eof,
    /// The application dropped the body before its end.
    Dropped,
    /// The body ended with an error.
    Failed,
    /// The call's cancellation took the body away.
    Aborted,
    /// The call failed terminally (trap or timeout) before the body ended; whatever the guest
    /// wrote so far is not a complete response.
    Broken,
    /// The handler returned no response.
    Absent,
}

/// State shared by one call's Store side and the application's two bodies.
pub(crate) struct Exchange {
    /// The request body ends with `connection-terminated` once this is set.
    request_closed: AtomicBool,
    request_waker: AtomicWaker,
    /// Upstream's guest body; the Store side may take it away on cancellation.
    response: Mutex<Option<UnsyncBoxBody<Bytes, HttpError>>>,
    response_waker: AtomicWaker,
    delivery: watch::Sender<Option<Delivery>>,
}

impl Exchange {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            request_closed: AtomicBool::new(false),
            request_waker: AtomicWaker::new(),
            response: Mutex::new(None),
            response_waker: AtomicWaker::new(),
            delivery: watch::Sender::new(None),
        })
    }

    /// Record the first delivery outcome; anything but the end of the body closes the request.
    fn settle(&self, delivery: Delivery) {
        let first = self.delivery.send_if_modified(|current| {
            if current.is_some() {
                return false;
            }
            *current = Some(delivery);
            true
        });
        if first && delivery != Delivery::Eof {
            self.close_request();
        }
    }

    /// Whether the request body was closed for the guest.
    pub(crate) fn request_closed(&self) -> bool {
        self.request_closed.load(Ordering::Acquire)
    }

    fn close_request(&self) {
        self.request_closed.store(true, Ordering::Release);
        self.request_waker.wake();
    }

    /// Install the guest's response body before the head is published.
    /// Install the guest's response body, unless the call was already aborted: a response that
    /// arrives after a cancellation is refused and its body dropped here, so the guest's writer
    /// sees its reader gone and no application ever reads a body the call no longer delivers.
    fn respond(&self, body: UnsyncBoxBody<Bytes, HttpError>) -> bool {
        let mut slot = self.response.lock().expect("ingress response");
        if self.delivery.borrow().is_some() {
            drop(slot);
            drop(body);
            return false;
        }
        *slot = Some(body);
        true
    }

    /// The handler returned no response: nothing will be delivered.
    pub(crate) fn absent(&self) {
        self.settle(Delivery::Absent);
    }

    /// Cancellation: close the request, and drop the response body here so the guest's writer
    /// sees its reader gone; the application's next poll reports the abort.
    pub(crate) fn abort(&self) {
        // Settled under the slot's lock, so a concurrent `respond` sees the abort.
        let mut slot = self.response.lock().expect("ingress response");
        let body = slot.take();
        self.settle(Delivery::Aborted);
        drop(slot);
        drop(body);
        self.response_waker.wake();
    }

    /// A client disconnect reported by a platform carrier: drop the body now, as dropping a
    /// [`ResponseBody`] does, before the request closes, so the guest's next write fails.
    fn disconnect(&self) {
        let mut slot = self.response.lock().expect("ingress response");
        let body = slot.take();
        self.settle(Delivery::Dropped);
        drop(slot);
        drop(body);
        self.response_waker.wake();
    }

    /// A terminal call failure: take the body away before the Store (and with it the guest's
    /// stream) is dropped, so the application reads an error instead of a truncated end.
    pub(crate) fn fail(&self) {
        let mut slot = self.response.lock().expect("ingress response");
        let body = slot.take();
        self.settle(Delivery::Broken);
        drop(slot);
        drop(body);
        self.response_waker.wake();
    }

    /// Resolve once delivery has ended, whichever way.
    pub(crate) fn delivered(&self) -> impl Future<Output = Delivery> + Send + 'static {
        let mut receiver = self.delivery.subscribe();
        async move {
            receiver
                .wait_for(Option::is_some)
                .await
                .map(|delivery| delivery.expect("delivered"))
                .unwrap_or(Delivery::Dropped)
        }
    }

    /// The guest's `response.new` transmission result: the request body's processing outcome
    /// joined with delivery. Only a body read to its end is a successful transmission.
    pub(crate) fn transmission(
        self: &Arc<Self>,
        request: impl Future<Output = std::result::Result<(), HttpError>> + Send + 'static,
    ) -> impl Future<Output = std::result::Result<(), HttpError>> + Send + 'static {
        let delivered = self.delivered();
        async move {
            let delivery = async move {
                match delivered.await {
                    Delivery::Eof => Ok(()),
                    _ => Err(HttpError::ConnectionTerminated),
                }
            };
            futures::future::try_join(request, delivery)
                .await
                .map(|_| ())
        }
    }
}

/// The application's request body, which the call can close on disconnect or cancellation.
struct RequestBody<B> {
    body: Pin<Box<B>>,
    exchange: Arc<Exchange>,
    ended: bool,
}

impl<B> Body for RequestBody<B>
where
    B: Body<Data = Bytes> + Send + 'static,
    B::Error: std::fmt::Display,
{
    type Data = Bytes;
    type Error = HttpError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<Option<std::result::Result<Frame<Bytes>, HttpError>>> {
        if self.ended {
            return Poll::Ready(None);
        }
        self.exchange.request_waker.register(cx.waker());
        if self.exchange.request_closed.load(Ordering::Acquire) {
            self.ended = true;
            return Poll::Ready(Some(Err(HttpError::ConnectionTerminated)));
        }
        match self.body.as_mut().poll_frame(cx) {
            Poll::Ready(Some(Err(error))) => {
                self.ended = true;
                Poll::Ready(Some(Err(HttpError::InternalError(Some(error.to_string())))))
            }
            Poll::Ready(None) => {
                self.ended = true;
                Poll::Ready(None)
            }
            Poll::Ready(Some(Ok(frame))) => Poll::Ready(Some(Ok(frame))),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// One response frame, or the failure that ended the body.
type BodyFrame = std::result::Result<Frame<Bytes>, BodyError>;

/// A failure while streaming the handler's response body.
#[derive(Debug)]
pub struct BodyError(String);

impl std::fmt::Display for BodyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for BodyError {}

/// The handler's response body. It may be polled on any thread while the Orb's Store drives the
/// guest that writes it; dropping it before its end signals a disconnect.
pub struct ResponseBody {
    exchange: Arc<Exchange>,
}

impl ResponseBody {
    /// Poll the next frame without recording the end of delivery yet: a carrier forwards the
    /// final frame first and only then settles, so physical completion never overtakes it.
    fn poll_deferred(
        &self,
        cx: &mut TaskContext<'_>,
    ) -> Poll<(Option<BodyFrame>, Option<Delivery>)> {
        let exchange = &self.exchange;
        exchange.response_waker.register(cx.waker());
        let mut response = exchange.response.lock().expect("ingress response");
        let Some(body) = response.as_mut() else {
            let frame = match *exchange.delivery.borrow() {
                Some(Delivery::Aborted) => {
                    Some(Err(BodyError("the handler call was cancelled".into())))
                }
                Some(Delivery::Broken) => Some(Err(BodyError(
                    "the handler call failed before its response ended".into(),
                ))),
                _ => None,
            };
            return Poll::Ready((frame, None));
        };
        let polled = std::task::ready!(Pin::new(body).poll_frame(cx));
        let ended = match &polled {
            None => Some(Delivery::Eof),
            Some(Err(_)) => Some(Delivery::Failed),
            Some(Ok(_)) => None,
        };
        if ended.is_some() {
            // Dropped here, outside the lock; delivery is recorded by the caller.
            let body = response.take();
            drop(response);
            drop(body);
        }
        Poll::Ready((
            polled.map(|frame| frame.map_err(|error| BodyError(error.to_string()))),
            ended,
        ))
    }

    /// Record the end of delivery reported by [`Self::poll_deferred`].
    fn delivered(&self, delivery: Delivery) {
        self.exchange.settle(delivery);
    }
}

impl Body for ResponseBody {
    type Data = Bytes;
    type Error = BodyError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<Option<std::result::Result<Frame<Bytes>, BodyError>>> {
        let (frame, ended) = std::task::ready!(self.poll_deferred(cx));
        if let Some(delivery) = ended {
            self.delivered(delivery);
        }
        Poll::Ready(frame)
    }
}

impl Drop for ResponseBody {
    /// A body dropped before its end is a disconnect: the guest learns it through its
    /// transmission future and its request body, and its next write is dropped.
    fn drop(&mut self) {
        let body = self
            .exchange
            .response
            .lock()
            .ok()
            .and_then(|mut body| body.take());
        self.exchange.settle(Delivery::Dropped);
        drop(body);
    }
}

pub(crate) type Head = std::result::Result<http::Response<ResponseBody>, tembra::Error>;

/// One ingress call's typed payload; it travels to the Store thread beside its command.
pub(crate) struct IngressCall {
    pub request: http::Request<UnsyncBoxBody<Bytes, HttpError>>,
    pub exchange: Arc<Exchange>,
    pub head: oneshot::Sender<Head>,
}

impl IngressCall {
    /// Publish the head with the guest's body; an application that already went away is a
    /// disconnect, and a call already aborted publishes its cancellation instead.
    pub(crate) fn publish(
        exchange: &Arc<Exchange>,
        head: oneshot::Sender<Head>,
        response: http::Response<UnsyncBoxBody<Bytes, HttpError>>,
    ) {
        let (parts, body) = response.into_parts();
        if !exchange.respond(body) {
            let _ = head.send(Err(tembra::Error::new(
                tembra::core::Code::Cancelled,
                "wasi_http_ingress",
                "the request was cancelled before its response",
            )));
            return;
        }
        let body = ResponseBody {
            exchange: exchange.clone(),
        };
        // A refused send drops the response with its body, which records the disconnect.
        let _ = head.send(Ok(http::Response::from_parts(parts, body)));
    }
}

/// The argument of the host-local `handle` method. Only [`serve`] can construct it, so no Orb or
/// foreign caller can forge an ingress call.
pub(crate) struct Ingress(pub(crate) IngressCall);

/// Physical completion of one served request; see the module documentation.
///
/// An owned observer of the root task that [`serve`] started: it borrows nothing, and dropping
/// it neither ends that root nor stops any physical work. It is not `Send`; poll it on the host's
/// thread. [`ResponseBody`] is the part that may move to another thread.
#[must_use = "dropping Settled stops nothing; await it to observe physical completion"]
pub struct Settled {
    completion: LocalBoxFuture<'static, Result<()>>,
    root: Root,
}

impl Settled {
    /// Cancel the request's root: the call's callback epoch ends, its request body is closed,
    /// its response body is taken away, and the call then settles. Cancelling a settled request
    /// does nothing.
    pub fn cancel(&self) {
        self.root.cancel();
    }
}

impl Future for Settled {
    type Output = Result<()>;
    /// Resolves at physical completion: a `Cancelled` error whenever the request's root was
    /// cancelled (by [`Settled::cancel`], by dropping `serve` early, or by `Host::close`),
    /// however the call then ended; otherwise `Ok` after a delivered, dropped or failed body, or
    /// the call's own error.
    fn poll(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Result<()>> {
        let result = std::task::ready!(self.completion.poll_unpin(cx));
        // A cancelled root reports the cancellation, whatever the call ended with while it
        // cooperated (a source guest's dispatch typically rejects with "callback cancelled").
        Poll::Ready(self.root.check().and(result))
    }
}

/// The request's root, as issued to its task; empty until the task first runs.
#[derive(Clone, Default)]
struct Root(Rc<std::cell::RefCell<Option<(Context, tembra::Cancellation)>>>);

impl Root {
    fn cancel(&self) {
        if let Some((context, _)) = self.0.borrow().as_ref() {
            let _ = context.cancel();
        }
    }

    /// The root's cancellation, observed through the signal captured while it was active.
    fn check(&self) -> Result<()> {
        match self.0.borrow().as_ref() {
            Some((_, signal)) if signal.is_cancelled() => Err(tembra::Error::new(
                tembra::core::Code::Cancelled,
                "wasi_http_ingress",
                "the request was cancelled",
            )),
            _ => Ok(()),
        }
    }
}

/// Cancels a request whose `serve` future is dropped before it returned: the request body
/// closes, a response the guest still produces is refused, and the root's cancellation releases
/// the call from admission or ends it, so an abandoned request does not hold its revision.
struct Pending {
    exchange: Arc<Exchange>,
    root: Root,
    abandoned: Rc<std::cell::Cell<bool>>,
    armed: bool,
}

impl Drop for Pending {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.abandoned.set(true);
        self.exchange.abort();
        self.root.cancel();
    }
}

/// Serve one request through the Orb behind `reference`, as `principal`.
///
/// The request runs under its own root, started as a task of `host` (see
/// [`Host::spawn_root`](tembra::Host::spawn_root)). That root, and with it the guest's authority
/// for dependency calls and outgoing requests, lasts until the call has physically ended, not
/// until this function returns: the headers can be handed to a server while the body streams.
/// The host's executor must keep running, as for every call; `Host::close` waits for the root.
///
/// Returns as soon as the handler produced its response; see the module documentation for the
/// body, disconnect, cancellation, timeout and error rules. Request body errors reach the guest
/// as `internal-error`. Dropping this future before it returns cancels the request.
pub async fn serve<B>(
    host: &Host,
    principal: &str,
    reference: Rc<Reference>,
    request: http::Request<B>,
) -> Result<(http::Response<ResponseBody>, Settled)>
where
    B: Body<Data = Bytes> + Send + 'static,
    B::Error: std::fmt::Display,
{
    let exchange = Exchange::new();
    let (head, published) = oneshot::channel();
    let request = request.map(|body| {
        RequestBody {
            body: Box::pin(body),
            exchange: exchange.clone(),
            ended: false,
        }
        .boxed_unsync()
    });
    let mut pending = Pending {
        exchange: exchange.clone(),
        root: Root::default(),
        abandoned: Rc::default(),
        armed: true,
    };
    let ingress = Ingress(IngressCall {
        request,
        exchange,
        head,
    });
    let (root, abandoned) = (pending.root.clone(), pending.abandoned.clone());
    let completion = host.spawn_root(principal, move |context| {
        *root.0.borrow_mut() = Some((context.clone(), context.signal()));
        if abandoned.get() {
            let _ = context.cancel();
        }
        async move {
            reference
                .call::<Ingress, ()>(&context, HANDLE, ingress)
                .await
        }
        .boxed_local()
    })?;
    let mut completion = async move {
        completion.await.unwrap_or_else(|_| {
            Err(crate::failure(
                "the host executor stopped before the request settled",
            ))
        })
    }
    .boxed_local();
    let result = match futures::future::select(published, completion.as_mut()).await {
        Either::Left((Ok(Ok(response)), _)) => Ok((response, completion)),
        // The handler answered with an error code or produced no response: report it once the
        // call has physically settled.
        // The completion carries the same refusal untyped; the head's error is the typed one.
        Either::Left((Ok(Err(error)), _)) => {
            let _ = completion.await;
            Err(error)
        }
        Either::Left((Err(_), _)) => match completion.await {
            Ok(()) => Err(crate::failure(
                "the incoming HTTP handler produced no response",
            )),
            Err(error) => Err(error),
        },
        Either::Right((settled, mut published)) => match published.try_recv() {
            Ok(Ok(response)) => Ok((response, async move { settled }.boxed_local())),
            Ok(Err(error)) => Err(error),
            Err(_) => match settled {
                Ok(()) => Err(crate::failure(
                    "the incoming HTTP handler produced no response",
                )),
                Err(error) => Err(error),
            },
        },
    };
    // Only a handed-off response keeps the request alive past this function. An error was
    // reported after settlement, so the armed guard's cancellation is a no-op then.
    // A request whose root was cancelled before its head reports the cancellation, whatever the
    // guest answered while cooperating with it.
    let (response, completion) = match result {
        Ok(handed) => handed,
        Err(error) => return Err(pending.root.check().err().unwrap_or(error)),
    };
    pending.armed = false;
    Ok((
        response,
        Settled {
            completion,
            root: pending.root.clone(),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    /// A response body that records being dropped.
    struct Tracked(Arc<AtomicBool>);
    impl Body for Tracked {
        type Data = Bytes;
        type Error = HttpError;
        fn poll_frame(
            self: Pin<&mut Self>,
            _: &mut TaskContext<'_>,
        ) -> Poll<Option<std::result::Result<Frame<Bytes>, HttpError>>> {
            Poll::Pending
        }
    }
    impl Drop for Tracked {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    /// A response that arrives after the call was aborted is refused: its body is dropped at
    /// once and the head carries the cancellation, never a live body.
    #[test]
    fn a_response_after_an_abort_is_refused_and_dropped() {
        let exchange = Exchange::new();
        exchange.abort();
        let dropped = Arc::new(AtomicBool::new(false));
        let (head, published) = oneshot::channel();
        IngressCall::publish(
            &exchange,
            head,
            http::Response::new(Tracked(dropped.clone()).boxed_unsync()),
        );
        assert!(dropped.load(Ordering::Acquire));
        let error = futures::executor::block_on(published)
            .unwrap()
            .err()
            .unwrap();
        assert_eq!(error.code, tembra::core::Code::Cancelled);
        assert!(exchange.request_closed());
    }

    /// A response that arrives first is installed; a later abort takes it away from the reader.
    #[test]
    fn an_abort_after_the_head_takes_the_body_away() {
        let exchange = Exchange::new();
        let dropped = Arc::new(AtomicBool::new(false));
        let (head, published) = oneshot::channel();
        IngressCall::publish(
            &exchange,
            head,
            http::Response::new(Tracked(dropped.clone()).boxed_unsync()),
        );
        let mut body = futures::executor::block_on(published)
            .unwrap()
            .unwrap()
            .into_body();
        assert!(!dropped.load(Ordering::Acquire));
        exchange.abort();
        assert!(dropped.load(Ordering::Acquire));
        let frame = futures::executor::block_on(body.frame()).unwrap();
        assert!(frame.unwrap_err().to_string().contains("cancelled"));
    }
}
