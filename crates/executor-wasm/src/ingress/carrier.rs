//! The typed incoming HTTP carrier of platform ports (Node and mobile).
//!
//! A platform host has no Rust `http` body to hand over. It supplies the request head as typed
//! raw bytes, answers the native side's request-body pulls one chunk at a time, and pulls the
//! response one frame at a time. Exactly one pull may be pending in each direction, which gives
//! backpressure without buffering beyond one chunk. JSON stays control-only; bodies, fields and
//! trailers are bytes.
//!
//! Everything else is the same call as the native facade: the same [`Exchange`], the same
//! response delivery rules and the same Store-side settlement. A carrier thread owns the
//! response body between pulls and forwards each frame before recording the end of delivery, so
//! the call's completion event can never overtake the last frame.
use super::{Exchange, Head, IngressCall, RequestBody};
use crate::worker::Event;
use bytes::Bytes;
use futures::{StreamExt, channel::mpsc};
use http_body::{Body, Frame};
use http_body_util::BodyExt;
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
};
use tembra::Result;
use tokio::sync::oneshot;

/// One field with its raw value bytes; order and duplicates are significant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IngressField {
    pub name: String,
    pub value: Vec<u8>,
}

/// The request head a platform host supplies.
///
/// `scheme` and `authority` form the request target's absolute part and are supplied together
/// or not at all: the guest's request carries an `http::Uri`, which has both or neither. A head
/// with only one of them is refused when the exchange begins. A host that only knows the
/// `Host` header supplies neither and leaves that header in `fields`.
#[derive(Clone, Debug)]
pub struct IngressRequestHead {
    pub method: String,
    /// `http`, `https` or another scheme; present exactly when `authority` is.
    pub scheme: Option<String>,
    /// `host[:port]`; present exactly when `scheme` is.
    pub authority: Option<String>,
    /// The raw origin-form target.
    pub path_with_query: Option<String>,
    pub fields: Vec<IngressField>,
}

/// The guest's response head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IngressResponseHead {
    pub status: u16,
    pub fields: Vec<IngressField>,
}

/// The platform's answer to one request-body pull.
#[derive(Clone, Debug)]
pub enum IngressRequestChunk {
    Data(Vec<u8>),
    /// The end of the request body, with its trailers (empty for none).
    End {
        trailers: Vec<IngressField>,
    },
    /// The request body failed; the guest sees `internal-error` with this message.
    Error(String),
}

/// The answer to one response pull.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IngressResponseFrame {
    Data(Vec<u8>),
    Trailers(Vec<IngressField>),
    /// The complete end of the body (its contents and trailers ended normally).
    End,
    /// The body failed; never a clean end.
    Error(String),
}

/// Incoming HTTP events of a platform exchange, beside its ordinary host requests.
#[derive(Debug)]
pub enum IngressEvent {
    /// The guest returned its response; sent once, only for `ok(response)`.
    Head(IngressResponseHead),
    /// The guest reads the request body: answer once with [`IngressPort::supply_request`].
    RequestPull,
    /// The answer to one [`IngressPort::pull_response`].
    Response(IngressResponseFrame),
    /// The guest returned its `error-code` instead of a response: the WIT value in the
    /// standard binary codec (`wasi:http/client#send`'s error type). Sent at most once, never
    /// with a `Head`, on the call's own event channel before the call completes.
    Refused(Vec<u8>),
}

/// Fields as a header map; an invalid name or value is refused.
fn header_map(fields: &[IngressField]) -> std::result::Result<http::HeaderMap, String> {
    let mut map = http::HeaderMap::with_capacity(fields.len());
    for field in fields {
        let name = http::HeaderName::from_bytes(field.name.as_bytes())
            .map_err(|_| format!("invalid field name {:?}", field.name))?;
        let value = http::HeaderValue::from_bytes(&field.value)
            .map_err(|_| format!("invalid value of field {:?}", field.name))?;
        map.append(name, value);
    }
    Ok(map)
}

/// A header map as typed fields, in its iteration order.
fn fields(map: &http::HeaderMap) -> Vec<IngressField> {
    map.iter()
        .map(|(name, value)| IngressField {
            name: name.as_str().to_owned(),
            value: value.as_bytes().to_vec(),
        })
        .collect()
}

/// The request-body supply shared by the Store-side body and the platform's port.
#[derive(Default)]
struct Supply {
    /// A pull was announced and not answered yet.
    pulled: bool,
    answer: Option<IngressRequestChunk>,
    waker: Option<Waker>,
    /// No further answer is accepted: the body ended or the exchange is over.
    closed: bool,
}

/// Announcing one pull while the event queue is full: it completes when the queue has room.
type Announcing = Pin<Box<dyn Future<Output = bool> + Send>>;

/// The request body as the Store reads it: each read pulls one chunk from the platform.
struct ForeignBody {
    supply: Arc<Mutex<Supply>>,
    events: async_channel::Sender<Event>,
    /// The current pull's announcement, delayed by a full event queue. The queue wakes this
    /// read when it has room; nothing spins and nothing is buffered beyond this one event.
    announcing: Option<Announcing>,
    ended: bool,
}

impl Body for ForeignBody {
    type Data = Bytes;
    type Error = String;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<std::result::Result<Frame<Bytes>, String>>> {
        if self.ended {
            return Poll::Ready(None);
        }
        if let Some(announcing) = self.announcing.as_mut() {
            match announcing.as_mut().poll(cx) {
                // The platform has not seen this pull yet; the queue wakes us with room, and a
                // disconnect wakes us through the supply.
                Poll::Pending => {
                    let closed = self.supply.lock().expect("ingress supply").closed;
                    if !closed {
                        return Poll::Pending;
                    }
                    self.announcing = None;
                    self.ended = true;
                    return Poll::Ready(Some(Err("the request body is no longer supplied".into())));
                }
                Poll::Ready(sent) => {
                    self.announcing = None;
                    if !sent {
                        return self.stopped();
                    }
                }
            }
        }
        let mut supply = self.supply.lock().expect("ingress supply");
        if let Some(answer) = supply.answer.take() {
            supply.pulled = false;
            let frame = match answer {
                IngressRequestChunk::Data(bytes) => Some(Ok(Frame::data(Bytes::from(bytes)))),
                IngressRequestChunk::End { trailers } => {
                    supply.closed = true;
                    drop(supply);
                    self.ended = true;
                    match header_map(&trailers) {
                        Ok(map) if map.is_empty() => None,
                        Ok(map) => Some(Ok(Frame::trailers(map))),
                        Err(error) => Some(Err(error)),
                    }
                }
                IngressRequestChunk::Error(message) => {
                    supply.closed = true;
                    drop(supply);
                    self.ended = true;
                    Some(Err(message))
                }
            };
            return Poll::Ready(frame);
        }
        if supply.closed {
            drop(supply);
            self.ended = true;
            return Poll::Ready(Some(Err("the request body is no longer supplied".into())));
        }
        if !supply.pulled {
            // One announcement per requested chunk. A full queue only delays it; the supply
            // lock is held until this read's waker is registered, so an answer that follows
            // the announcement at once is never missed.
            supply.pulled = true;
            match self
                .events
                .try_send(Event::Ingress(IngressEvent::RequestPull))
            {
                Ok(()) => {}
                Err(async_channel::TrySendError::Full(event)) => {
                    let events = self.events.clone();
                    let mut announcing: Announcing =
                        Box::pin(async move { events.send(event).await.is_ok() });
                    match announcing.as_mut().poll(cx) {
                        Poll::Ready(true) => {}
                        Poll::Ready(false) => {
                            drop(supply);
                            return self.stopped();
                        }
                        Poll::Pending => {
                            supply.waker = Some(cx.waker().clone());
                            drop(supply);
                            self.announcing = Some(announcing);
                            return Poll::Pending;
                        }
                    }
                }
                Err(async_channel::TrySendError::Closed(_)) => {
                    drop(supply);
                    return self.stopped();
                }
            }
        }
        supply.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

impl ForeignBody {
    /// The platform no longer reads events: the body fails and accepts no further answer.
    fn stopped(&mut self) -> Poll<Option<std::result::Result<Frame<Bytes>, String>>> {
        if let Ok(mut supply) = self.supply.lock() {
            supply.closed = true;
            supply.answer = None;
        }
        self.ended = true;
        Poll::Ready(Some(Err("the platform stopped reading events".into())))
    }
}

impl Drop for ForeignBody {
    /// The guest no longer reads: later answers are refused and own nothing.
    fn drop(&mut self) {
        if let Ok(mut supply) = self.supply.lock() {
            supply.closed = true;
            supply.answer = None;
        }
    }
}

/// What the platform asks of the response carrier.
enum Pull {
    Next,
    Disconnect,
}

/// The platform's side of one incoming HTTP exchange. Dropping it is a disconnect.
pub struct IngressPort {
    supply: Arc<Mutex<Supply>>,
    exchange: Arc<Exchange>,
    pulls: mpsc::UnboundedSender<Pull>,
    /// A response pull is pending; the head has been published.
    state: Arc<Mutex<Pulling>>,
}

#[derive(Default)]
struct Pulling {
    headed: bool,
    pending: bool,
    ended: bool,
}

impl IngressPort {
    /// Answer the one pending request-body pull. Returns `false` when the answer is no longer
    /// wanted (the body ended, the guest dropped it, or the exchange is over); the chunk is then
    /// dropped and owns nothing. Answering without a pending pull is a protocol error.
    pub fn supply_request(&self, chunk: IngressRequestChunk) -> Result<bool> {
        let mut supply = self.supply.lock().map_err(crate::failure)?;
        // A closed request (disconnect, cancellation, no response) wants no further data.
        if supply.closed || self.exchange.request_closed() {
            supply.answer = None;
            return Ok(false);
        }
        if !supply.pulled || supply.answer.is_some() {
            return Err(crate::failure(
                "no request-body pull is pending for this answer",
            ));
        }
        supply.answer = Some(chunk);
        if let Some(waker) = supply.waker.take() {
            waker.wake();
        }
        Ok(true)
    }

    /// Ask for the next response frame; it arrives as one `Response` event. Only one pull may be
    /// pending, and only after the head and before the body's end.
    pub fn pull_response(&self) -> Result<()> {
        let mut state = self.state.lock().map_err(crate::failure)?;
        if !state.headed || state.pending || state.ended {
            return Err(crate::failure(
                "a response pull needs a published head, no pending pull and a live body",
            ));
        }
        state.pending = true;
        self.pulls
            .unbounded_send(Pull::Next)
            .map_err(|_| crate::failure("the response carrier has ended"))
    }

    /// The client went away: the response body is dropped, which the guest observes through its
    /// transmission result and its request body.
    pub fn disconnect(&self) {
        // Before the head there is no body to drop yet: abort the exchange instead, so a late
        // response is refused and the guest's request read ends (as for a dropped `serve`).
        let headed = self.state.lock().map(|state| state.headed).unwrap_or(true);
        if headed {
            self.exchange.disconnect();
        } else {
            self.exchange.abort();
        }
        let _ = self.pulls.unbounded_send(Pull::Disconnect);
        if let Ok(mut supply) = self.supply.lock() {
            supply.closed = true;
            supply.answer = None;
            // A read waiting for an answer ends now instead of at the call's timeout.
            if let Some(waker) = supply.waker.take() {
                waker.wake();
            }
        }
    }
}

impl Drop for IngressPort {
    fn drop(&mut self) {
        self.disconnect();
    }
}

/// Build the call's typed payload from a platform head and start its response carrier.
pub(crate) fn begin(
    head: IngressRequestHead,
    events: async_channel::Sender<Event>,
) -> Result<(IngressCall, IngressPort)> {
    let method = http::Method::from_bytes(head.method.as_bytes())
        .map_err(|_| crate::failure("invalid request method"))?;
    if head.scheme.is_some() != head.authority.is_some() {
        return Err(crate::failure(
            "a request target names its scheme and authority together, or neither",
        ));
    }
    let mut uri = http::Uri::builder();
    if let Some(scheme) = &head.scheme {
        uri = uri.scheme(scheme.as_str());
    }
    if let Some(authority) = &head.authority {
        uri = uri.authority(authority.as_str());
    }
    uri = uri.path_and_query(head.path_with_query.as_deref().unwrap_or("/"));
    let uri = uri
        .build()
        .map_err(|error| crate::failure(format!("invalid request target: {error}")))?;
    let headers = header_map(&head.fields).map_err(crate::failure)?;
    let supply = Arc::new(Mutex::new(Supply::default()));
    let exchange = Exchange::new();
    let body = ForeignBody {
        supply: supply.clone(),
        events: events.clone(),
        announcing: None,
        ended: false,
    };
    let mut request = http::Request::new(
        RequestBody {
            body: Box::pin(body),
            exchange: exchange.clone(),
            ended: false,
        }
        .boxed_unsync(),
    );
    *request.method_mut() = method;
    *request.uri_mut() = uri;
    *request.headers_mut() = headers;
    let call_exchange = exchange.clone();
    let (head, published) = oneshot::channel();
    let (pulls, requested) = mpsc::unbounded();
    let state = Arc::new(Mutex::new(Pulling::default()));
    let carrier = Carrier {
        events,
        state: state.clone(),
    };
    std::thread::Builder::new()
        .name("tembra-ingress".into())
        .spawn(move || futures::executor::block_on(carrier.run(published, requested)))
        .map_err(crate::failure)?;
    Ok((
        IngressCall {
            request,
            exchange,
            head,
        },
        IngressPort {
            supply,
            exchange: call_exchange,
            pulls,
            state,
        },
    ))
}

/// Owns the response body between pulls and forwards its frames as events.
struct Carrier {
    events: async_channel::Sender<Event>,
    state: Arc<Mutex<Pulling>>,
}

impl Carrier {
    async fn run(self, head: oneshot::Receiver<Head>, mut pulls: mpsc::UnboundedReceiver<Pull>) {
        // An error before the head is reported by the call's completion, after settlement.
        let Ok(Ok(response)) = head.await else {
            return;
        };
        let (parts, body) = response.into_parts();
        self.state.lock().expect("ingress pulling").headed = true;
        let head = IngressResponseHead {
            status: parts.status.as_u16(),
            fields: fields(&parts.headers),
        };
        // An exchange nobody reads any more drops the body: a disconnect.
        if self
            .events
            .send(Event::Ingress(IngressEvent::Head(head)))
            .await
            .is_err()
        {
            return;
        }
        while let Some(Pull::Next) = pulls.next().await {
            let (frame, ended) = std::future::poll_fn(|cx| body.poll_deferred(cx)).await;
            let (frame, terminal) = match frame {
                Some(Ok(frame)) => match frame.into_data() {
                    Ok(data) => (IngressResponseFrame::Data(data.to_vec()), false),
                    Err(frame) => match frame.into_trailers() {
                        Ok(trailers) => (IngressResponseFrame::Trailers(fields(&trailers)), false),
                        Err(_) => (
                            IngressResponseFrame::Error("unknown response frame".into()),
                            true,
                        ),
                    },
                },
                Some(Err(error)) => (IngressResponseFrame::Error(error.to_string()), true),
                None => (IngressResponseFrame::End, true),
            };
            {
                let mut state = self.state.lock().expect("ingress pulling");
                state.pending = false;
                state.ended = terminal;
            }
            let sent = self
                .events
                .send(Event::Ingress(IngressEvent::Response(frame)))
                .await
                .is_ok();
            // Only now may the call complete: its last frame is already queued before it.
            if let Some(delivery) = ended {
                body.delivered(delivery);
            }
            if terminal || !sent {
                return;
            }
        }
        // A disconnect, or a port dropped by the platform: dropping the body records it.
        drop(body);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(name: &str, value: &[u8]) -> IngressField {
        IngressField {
            name: name.into(),
            value: value.to_vec(),
        }
    }

    /// A port and the Store-side body it feeds, without a Store.
    fn pair() -> (IngressPort, ForeignBody, async_channel::Receiver<Event>) {
        queued(4)
    }

    /// A pair whose event queue holds `capacity` events.
    fn queued(capacity: usize) -> (IngressPort, ForeignBody, async_channel::Receiver<Event>) {
        let (events, receive) = async_channel::bounded(capacity);
        let supply = Arc::new(Mutex::new(Supply::default()));
        let (pulls, _requested) = mpsc::unbounded();
        let port = IngressPort {
            supply: supply.clone(),
            exchange: Exchange::new(),
            pulls,
            state: Arc::default(),
        };
        let body = ForeignBody {
            supply,
            events,
            announcing: None,
            ended: false,
        };
        (port, body, receive)
    }

    fn poll(body: &mut ForeignBody) -> Poll<Option<std::result::Result<Frame<Bytes>, String>>> {
        let waker = futures::task::noop_waker();
        Pin::new(body).poll_frame(&mut Context::from_waker(&waker))
    }

    #[test]
    fn one_answer_per_announced_pull_and_late_answers_own_nothing() {
        let (port, mut body, events) = pair();
        // No pull announced yet.
        assert!(
            port.supply_request(IngressRequestChunk::Data(vec![1]))
                .is_err()
        );
        assert!(poll(&mut body).is_pending());
        assert!(matches!(
            events.try_recv(),
            Ok(Event::Ingress(IngressEvent::RequestPull))
        ));
        // A second poll while waiting announces nothing new.
        assert!(poll(&mut body).is_pending());
        assert!(events.try_recv().is_err());
        assert!(
            port.supply_request(IngressRequestChunk::Data(vec![1, 0xff]))
                .unwrap()
        );
        // A second answer to the same pull is refused.
        assert!(
            port.supply_request(IngressRequestChunk::Data(vec![2]))
                .is_err()
        );
        let Poll::Ready(Some(Ok(frame))) = poll(&mut body) else {
            panic!("expected data")
        };
        assert_eq!(frame.into_data().unwrap().as_ref(), &[1, 0xff]);
        // The guest pulls again, then drops the body: the late answer is not accepted.
        assert!(poll(&mut body).is_pending());
        drop(body);
        assert!(
            !port
                .supply_request(IngressRequestChunk::Data(vec![3]))
                .unwrap()
        );
    }

    #[test]
    fn trailers_keep_duplicates_and_raw_bytes_and_end_the_body() {
        let (port, mut body, _events) = pair();
        assert!(poll(&mut body).is_pending());
        let trailers = vec![
            field("x-a", b"1"),
            field("x-a", b"\x802"),
            field("x-b", b""),
        ];
        assert!(
            port.supply_request(IngressRequestChunk::End {
                trailers: trailers.clone()
            })
            .unwrap()
        );
        let Poll::Ready(Some(Ok(frame))) = poll(&mut body) else {
            panic!("expected trailers")
        };
        assert_eq!(fields(&frame.into_trailers().unwrap()), trailers);
        assert!(matches!(poll(&mut body), Poll::Ready(None)));
        // Nothing is accepted after the end.
        assert!(
            !port
                .supply_request(IngressRequestChunk::Data(vec![1]))
                .unwrap()
        );
    }

    #[test]
    fn a_disconnected_port_accepts_no_request_data() {
        let (port, mut body, _events) = pair();
        assert!(poll(&mut body).is_pending());
        port.disconnect();
        assert!(
            !port
                .supply_request(IngressRequestChunk::Data(vec![1]))
                .unwrap()
        );
        assert!(matches!(poll(&mut body), Poll::Ready(Some(Err(_)))));
    }

    /// A body that ends only after a few pending polls, so EOF is observed late.
    struct Slow(usize);
    impl Body for Slow {
        type Data = Bytes;
        type Error = crate::ingress::HttpError;
        fn poll_frame(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Option<std::result::Result<Frame<Bytes>, Self::Error>>> {
            if self.0 == 0 {
                return Poll::Ready(None);
            }
            self.0 -= 1;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }

    /// The end of delivery (which lets the Store publish the call's completion) is recorded only
    /// after the terminal frame is already queued on the same event stream, so completion can
    /// never overtake it, however the carrier and the Store are scheduled.
    #[test]
    fn the_terminal_frame_is_queued_before_delivery_ends() {
        for pending in [0, 1, 5] {
            let (events, receive) = async_channel::bounded(8);
            let exchange = Exchange::new();
            let (head, published) = oneshot::channel();
            let (pulls, requested) = mpsc::unbounded();
            let state = Arc::new(Mutex::new(Pulling::default()));
            let carrier = Carrier {
                events,
                state: state.clone(),
            };
            let response = http::Response::new(Slow(pending).boxed_unsync());
            IngressCall::publish(&exchange, head, response);
            let delivered = exchange.delivered();
            let observed = receive.clone();
            let mut pool = futures::executor::LocalPool::new();
            let spawner = pool.spawner();
            futures::task::LocalSpawnExt::spawn_local(&spawner, carrier.run(published, requested))
                .unwrap();
            let checked = std::rc::Rc::new(std::cell::Cell::new(false));
            let flag = checked.clone();
            futures::task::LocalSpawnExt::spawn_local(&spawner, async move {
                assert_eq!(delivered.await, crate::ingress::Delivery::Eof);
                // At the instant delivery ends, the head and the terminal frame are queued.
                let queued: Vec<_> = std::iter::from_fn(|| observed.try_recv().ok()).collect();
                assert!(matches!(
                    queued.last(),
                    Some(Event::Ingress(IngressEvent::Response(
                        IngressResponseFrame::End
                    )))
                ));
                flag.set(true);
            })
            .unwrap();
            pool.run_until_stalled();
            state.lock().unwrap().pending = true;
            pulls.unbounded_send(Pull::Next).unwrap();
            pool.run_until_stalled();
            assert!(
                checked.get(),
                "delivery did not end after {pending} pending polls"
            );
        }
    }

    /// Counts the wakes of the read that registered it.
    #[derive(Default)]
    struct Wakes(std::sync::atomic::AtomicUsize);
    impl futures::task::ArcWake for Wakes {
        fn wake_by_ref(this: &Arc<Self>) {
            this.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn poll_with(
        body: &mut ForeignBody,
        wakes: &Arc<Wakes>,
    ) -> Poll<Option<std::result::Result<Frame<Bytes>, String>>> {
        let waker = futures::task::waker(wakes.clone());
        Pin::new(body).poll_frame(&mut Context::from_waker(&waker))
    }

    /// An unrelated event that fills the queue.
    fn filler() -> Event {
        Event::Done(Ok("null".into()))
    }

    fn is_pull(event: Option<Event>) -> bool {
        matches!(event, Some(Event::Ingress(IngressEvent::RequestPull)))
    }

    #[test]
    fn a_full_event_queue_delays_the_pull_without_ending_the_body() {
        let (port, mut body, events) = queued(1);
        let (sender, wakes) = (body.events.clone(), Arc::new(Wakes::default()));
        sender.try_send(filler()).unwrap();
        // The pull waits for room instead of failing the body.
        assert!(poll_with(&mut body, &wakes).is_pending());
        assert!(poll_with(&mut body, &wakes).is_pending());
        assert!(matches!(events.try_recv(), Ok(Event::Done(_))));
        // Room wakes the read, which announces its one pull.
        assert!(wakes.0.load(std::sync::atomic::Ordering::SeqCst) >= 1);
        assert!(poll_with(&mut body, &wakes).is_pending());
        assert!(is_pull(events.try_recv().ok()));
        assert!(poll_with(&mut body, &wakes).is_pending());
        assert!(events.try_recv().is_err(), "one announcement per pull");
        assert!(
            port.supply_request(IngressRequestChunk::Data(vec![7]))
                .unwrap()
        );
        let Poll::Ready(Some(Ok(frame))) = poll_with(&mut body, &wakes) else {
            panic!("expected data")
        };
        assert_eq!(frame.into_data().unwrap().as_ref(), &[7]);
        // The next pull is announced at once while the queue has room.
        assert!(poll_with(&mut body, &wakes).is_pending());
        assert!(is_pull(events.try_recv().ok()));
    }

    #[test]
    fn a_closed_event_queue_refuses_the_pull() {
        let (port, mut body, events) = queued(1);
        drop(events);
        let Poll::Ready(Some(Err(message))) = poll(&mut body) else {
            panic!("expected a refusal")
        };
        assert_eq!(message, "the platform stopped reading events");
        assert!(matches!(poll(&mut body), Poll::Ready(None)));
        assert!(
            !port
                .supply_request(IngressRequestChunk::Data(vec![1]))
                .unwrap()
        );
        // A queue that closes while the pull waits for room is refused the same way.
        let (_port, mut body, events) = queued(1);
        let wakes = Arc::new(Wakes::default());
        body.events.try_send(filler()).unwrap();
        assert!(poll_with(&mut body, &wakes).is_pending());
        drop(events);
        assert!(wakes.0.load(std::sync::atomic::Ordering::SeqCst) >= 1);
        let Poll::Ready(Some(Err(message))) = poll_with(&mut body, &wakes) else {
            panic!("expected a refusal")
        };
        assert_eq!(message, "the platform stopped reading events");
    }

    #[test]
    fn a_disconnect_ends_a_read_waiting_for_queue_room() {
        let (port, mut body, _events) = queued(1);
        let wakes = Arc::new(Wakes::default());
        body.events.try_send(filler()).unwrap();
        assert!(poll_with(&mut body, &wakes).is_pending());
        port.disconnect();
        assert!(wakes.0.load(std::sync::atomic::Ordering::SeqCst) >= 1);
        assert!(matches!(
            poll_with(&mut body, &wakes),
            Poll::Ready(Some(Err(_)))
        ));
        assert!(matches!(poll(&mut body), Poll::Ready(None)));
    }

    fn head(scheme: Option<&str>, authority: Option<&str>) -> IngressRequestHead {
        IngressRequestHead {
            method: "GET".into(),
            scheme: scheme.map(str::to_owned),
            authority: authority.map(str::to_owned),
            path_with_query: Some("/a?b".into()),
            fields: vec![field("host", b"example.test")],
        }
    }

    #[test]
    fn scheme_and_authority_are_supplied_together_or_not_at_all() {
        let (events, _receive) = async_channel::bounded(4);
        for (scheme, authority) in [(Some("https"), None), (None, Some("example.test"))] {
            let error = begin(head(scheme, authority), events.clone())
                .err()
                .expect("a one-sided request target is refused");
            assert!(
                error.message.contains("scheme and authority together"),
                "{error}"
            );
        }
        let (call, _port) = begin(
            head(Some("https"), Some("example.test:8443")),
            events.clone(),
        )
        .unwrap();
        assert_eq!(
            call.request.uri().to_string(),
            "https://example.test:8443/a?b"
        );
        let (call, _port) = begin(head(None, None), events).unwrap();
        assert_eq!(call.request.uri().to_string(), "/a?b");
        assert_eq!(call.request.headers()["host"], "example.test");
    }

    #[test]
    fn invalid_field_bytes_are_refused() {
        assert!(header_map(&[field("x", b"a\nb")]).is_err());
        assert!(header_map(&[field("bad name", b"v")]).is_err());
        assert_eq!(
            fields(&header_map(&[field("x", b"\xfe")]).unwrap()),
            vec![field("x", b"\xfe")]
        );
    }
}
