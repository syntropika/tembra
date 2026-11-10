//! Byte stream results at the native portable boundary: logical head versus physical completion.
//!
//! The provider stands in for a stream-issuing executor. Its `payload_method` returns an
//! `Outcome` whose head carries one stream end over a bounded test source, and whose physical
//! work is a producer task writing into that source. Everything runs on the real kernel,
//! `Host::root` and the host's managed executor; no executor, Wasm or contract is involved.
use futures::{
    FutureExt, StreamExt,
    channel::oneshot,
    executor::LocalPool,
    future::{AbortHandle, Abortable, poll_fn},
    task::LocalSpawnExt,
};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    rc::Rc,
    task::{Context, Poll, Waker},
};
use tembra::{
    Host, Orb, Reference, Service, core,
    portable::{
        ByteSource, ByteStream, Capability, Delivered, FromPayload, Issued, Nominal, Origin,
        Outcome, Owned, Payload,
    },
};

const CONSTRUCTOR: &str = "[constructor]decoder";
const DECODE: &str = "[method]decoder.frames";

thread_local! {
    /// The host whose live calls a closing decoder records.
    static OBSERVED: RefCell<Option<Host>> = const { RefCell::new(None) };
}

/// A resource's issuer backend; dropping it is the resource's close.
struct Decoder {
    nominal: Nominal,
    log: Rc<DecoderLog>,
}

#[derive(Default)]
struct DecoderLog {
    closed: Cell<u32>,
    calls_at_close: Cell<Option<usize>>,
}

impl Capability for Decoder {
    fn nominal(&self) -> &Nominal {
        &self.nominal
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        self.log.closed.set(self.log.closed.get() + 1);
        let calls = OBSERVED.with(|host| {
            host.borrow()
                .as_ref()
                .map(|host| host.kernel().diagnostics().calls)
        });
        self.log.calls_at_close.set(calls);
    }
}

/// Chunks the source buffers before the producer must wait.
const CAPACITY: usize = 2;

/// The bounded buffer between the producer task (physical work) and the stream end.
#[derive(Default)]
struct Channel {
    buffer: VecDeque<Vec<u8>>,
    produced: u32,
    ended: bool,
    failure: Option<String>,
    /// Close requests the source received.
    close_signals: u32,
    /// The producer stopped physically, with its outcome.
    stopped: Option<tembra::Result<()>>,
    reader: Option<Waker>,
    producer: Option<Waker>,
    acknowledgement: Option<Waker>,
}

type Shared = Rc<RefCell<Channel>>;

fn wake(waker: &mut Option<Waker>) {
    if let Some(waker) = waker.take() {
        waker.wake();
    }
}

/// The stream end's view of the channel, as an executor's pipe would provide it.
struct Source(Shared);

impl ByteSource for Source {
    fn poll_next(&mut self, context: &mut Context<'_>) -> Poll<Option<tembra::Result<Vec<u8>>>> {
        let mut channel = self.0.borrow_mut();
        if let Some(chunk) = channel.buffer.pop_front() {
            wake(&mut channel.producer);
            return Poll::Ready(Some(Ok(chunk)));
        }
        if let Some(failure) = channel.failure.clone() {
            return Poll::Ready(Some(Err(tembra::Error::new(
                core::Code::Unavailable,
                "producer",
                failure,
            ))));
        }
        if channel.ended {
            return Poll::Ready(None);
        }
        channel.reader = Some(context.waker().clone());
        Poll::Pending
    }

    fn close(&mut self) {
        let mut channel = self.0.borrow_mut();
        channel.close_signals += 1;
        wake(&mut channel.producer);
    }

    fn poll_closed(&mut self, context: &mut Context<'_>) -> Poll<tembra::Result<()>> {
        let mut channel = self.0.borrow_mut();
        match &channel.stopped {
            Some(outcome) => Poll::Ready(outcome.clone()),
            None => {
                channel.acknowledgement = Some(context.waker().clone());
                Poll::Pending
            }
        }
    }
}

/// How the next `frames` call behaves; every gate is optional.
#[derive(Default)]
struct Plan {
    /// Held before the provider returns its head.
    entry: Option<oneshot::Receiver<()>>,
    /// Held before the producer writes anything.
    begin: Option<oneshot::Receiver<()>>,
    /// Held after the producer stopped writing, before it physically stops.
    finish: Option<oneshot::Receiver<()>>,
    /// The producer's physical work fails with this message after writing.
    fail: Option<String>,
}

#[derive(Default)]
struct Producer {
    plan: RefCell<Plan>,
    channel: RefCell<Option<Shared>>,
    decoders: Rc<DecoderLog>,
}

/// One stream head over a fresh channel, with the producer as its physical work.
async fn stream_outcome(producer: &Producer, count: u32) -> tembra::Result<Outcome> {
    let mut plan = std::mem::take(&mut *producer.plan.borrow_mut());
    if let Some(entry) = plan.entry.take() {
        let _ = entry.await;
    }
    let channel: Shared = Rc::default();
    *producer.channel.borrow_mut() = Some(channel.clone());
    let mut head = Payload::data(Value::Null);
    let index = head.attach_stream(ByteStream::new(Source(channel.clone())));
    let head = head.map_value(|_| Ok(json!(index)));
    Ok(Outcome::continuing(head, produce(channel, count, plan)))
}

/// Physical work: write `count` chunks into the bounded channel, stop at a close request.
async fn produce(channel: Shared, count: u32, plan: Plan) -> tembra::Result<()> {
    if let Some(begin) = plan.begin {
        let _ = begin.await;
    }
    for index in 0..count {
        let room = poll_fn(|context| {
            let mut state = channel.borrow_mut();
            if state.close_signals > 0 {
                return Poll::Ready(false);
            }
            if state.buffer.len() < CAPACITY {
                return Poll::Ready(true);
            }
            state.producer = Some(context.waker().clone());
            Poll::Pending
        })
        .await;
        if !room {
            break;
        }
        let mut state = channel.borrow_mut();
        state.buffer.push_back(vec![index as u8; 4]);
        state.produced += 1;
        wake(&mut state.reader);
    }
    let outcome = match plan.fail {
        Some(failure) => {
            let mut state = channel.borrow_mut();
            state.failure = Some(failure.clone());
            wake(&mut state.reader);
            Err(tembra::Error::new(
                core::Code::Unavailable,
                "producer",
                failure,
            ))
        }
        None => {
            let mut state = channel.borrow_mut();
            state.ended = true;
            wake(&mut state.reader);
            Ok(())
        }
    };
    if let Some(finish) = plan.finish {
        let _ = finish.await;
    }
    let mut state = channel.borrow_mut();
    state.stopped = Some(outcome.clone());
    wake(&mut state.acknowledgement);
    outcome
}

fn service(producer: Rc<Producer>) -> Service<Rc<Producer>> {
    Service::new("frames", move |_| Ok(producer.clone()))
        .payload_method(
            "frames",
            |producer: Rc<Rc<Producer>>, _, payload: Payload| async move {
                let count = payload.into_data()?[0].as_u64().unwrap_or(0) as u32;
                stream_outcome(&producer, count).await
            },
        )
        // A data head whose physical work then fails.
        .payload_method("late", |_, _, _: Payload| async move {
            Ok(Outcome::continuing(Ok(Payload::data(json!(1))), async {
                Err(tembra::Error::new(
                    core::Code::Unavailable,
                    "producer",
                    "cleanup failed",
                ))
            }))
        })
        .payload_method(CONSTRUCTOR, |producer: Rc<Rc<Producer>>, _, _: Payload| async move {
            let mut issued = Payload::data(Value::Null);
            let index = issued.attach(Issued::new(Decoder {
                nominal: Nominal {
                    interface: "test:media/codecs@1.0.0".into(),
                    name: "decoder".into(),
                },
                log: producer.decoders.clone(),
            }));
            issued.map_value(|_| Ok(json!(index)))
        })
        // A resource method whose stream keeps working after its head.
        .payload_method(DECODE, |producer: Rc<Rc<Producer>>, _, mut payload: Payload| async move {
            payload.take_receiver();
            stream_outcome(&producer, 100).await
        })
        .method(
            "scalar",
            |_, _, (value,): (u32,)| async move { Ok(value + 1) },
        )
}

struct Fixture {
    pool: LocalPool,
    host: Host,
    producer: Rc<Producer>,
    reference: Rc<Reference>,
}

fn fixture() -> Fixture {
    let pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let producer = Rc::new(Producer::default());
    host.install(vec![
        Orb::new("media", 1).service(service(producer.clone())),
    ])
    .unwrap();
    let reference = Rc::new(host.get("frames", vec!["main".into()], "host").unwrap());
    Fixture {
        pool,
        host,
        producer,
        reference,
    }
}

impl Fixture {
    fn root<T: 'static>(
        &mut self,
        operation: impl AsyncFnOnce(tembra::Context, Rc<Reference>) -> tembra::Result<T> + 'static,
    ) -> tembra::Result<T> {
        let reference = self.reference.clone();
        self.pool.run_until(self.host.root("host", move |context| {
            operation(context, reference).boxed_local()
        }))
    }

    fn channel(&self) -> Shared {
        self.producer
            .channel
            .borrow()
            .clone()
            .expect("a stream was issued")
    }

    fn calls(&self) -> usize {
        self.host.kernel().diagnostics().calls
    }

    /// Release the reference and close the host; every kernel record must be gone.
    fn close(mut self) {
        self.reference.release().unwrap();
        self.pool.run_until(self.host.close()).unwrap();
        let diagnostics = self.host.kernel().diagnostics();
        assert_eq!(
            (diagnostics.calls, diagnostics.references, diagnostics.roots),
            (0, 0, 0)
        );
        assert!(self.host.errors().is_empty(), "{:?}", self.host.errors());
    }
}

fn gate() -> (oneshot::Sender<()>, oneshot::Receiver<()>) {
    oneshot::channel()
}

#[test]
fn the_head_arrives_while_the_call_is_physically_admitted_and_reads_are_bounded() {
    let mut fixture = fixture();
    let (begin, held) = gate();
    fixture.producer.plan.borrow_mut().begin = Some(held);
    let host = fixture.host.clone();
    let producer = fixture.producer.clone();
    let (calls_at_head, received, bound_held) = fixture
        .root(async move |context, reference| {
            let mut stream = reference
                .call_portable::<_, ByteStream>(&context, "frames", (6_u32,))
                .await?;
            // The logical head is here; the producer has not written and its call is admitted.
            let channel = producer.channel.borrow().clone().unwrap();
            let calls_at_head = host.kernel().diagnostics().calls;
            assert_eq!(channel.borrow().produced, 0);
            let _ = begin.send(());
            let mut received = Vec::new();
            let mut bound_held = true;
            while let Some(chunk) = stream.next().await {
                received.push(chunk?[0]);
                // The producer never runs more than the buffer ahead of the reader.
                bound_held &= channel.borrow().produced as usize <= received.len() + CAPACITY;
            }
            Ok((calls_at_head, received, bound_held))
        })
        .unwrap();
    assert_eq!(calls_at_head, 1, "the call stays admitted after its head");
    assert_eq!(received, vec![0, 1, 2, 3, 4, 5]);
    assert!(bound_held);
    fixture.pool.run_until_stalled();
    assert_eq!(fixture.calls(), 0);
    // A clean end needs no close; dropping the end afterwards signals it exactly once.
    assert_eq!(fixture.channel().borrow().close_signals, 1);
    fixture.close();
}

#[test]
fn an_explicit_close_returns_only_after_the_producer_physically_stopped() {
    let mut fixture = fixture();
    let (finish, held) = gate();
    {
        let mut plan = fixture.producer.plan.borrow_mut();
        plan.finish = Some(held);
        plan.fail = Some("producer timed out".into());
    }
    let order = Rc::new(RefCell::new(Vec::new()));
    // Released only once the close request reached the producer.
    let releaser = {
        let (producer, order) = (fixture.producer.clone(), order.clone());
        async move {
            poll_fn(|context| {
                let signalled = producer
                    .channel
                    .borrow()
                    .as_ref()
                    .is_some_and(|channel| channel.borrow().close_signals == 1);
                if signalled {
                    Poll::Ready(())
                } else {
                    context.waker().wake_by_ref();
                    Poll::Pending
                }
            })
            .await;
            order.borrow_mut().push("finish released");
            let _ = finish.send(());
        }
    };
    fixture.pool.spawner().spawn_local(releaser).unwrap();
    let recorded = order.clone();
    let closed = fixture
        .root(async move |context, reference| {
            let mut stream = reference
                .call_portable::<_, ByteStream>(&context, "frames", (100_u32,))
                .await?;
            stream.next().await.unwrap()?;
            recorded.borrow_mut().push("close requested");
            let closed = stream.close().await;
            recorded.borrow_mut().push("close returned");
            Ok(closed)
        })
        .unwrap();
    // The acknowledgement carries the producer's own final outcome.
    assert_eq!(closed.unwrap_err().message, "producer timed out");
    assert_eq!(
        *order.borrow(),
        vec!["close requested", "finish released", "close returned"]
    );
    fixture.pool.run_until_stalled();
    assert_eq!(fixture.channel().borrow().close_signals, 1);
    assert!(fixture.channel().borrow().produced < 100);
    // The same failure is the call's physical outcome.
    let errors = fixture.host.errors();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].message, "producer timed out");
    fixture.close();
}

#[test]
fn dropping_or_refusing_a_stream_closes_it_once_while_physical_work_still_settles() {
    let mut fixture = fixture();
    // A consumer drop after one chunk.
    fixture
        .root(async move |context, reference| {
            let mut stream = reference
                .call_portable::<_, ByteStream>(&context, "frames", (100_u32,))
                .await?;
            stream.next().await.unwrap()?;
            Ok(())
        })
        .unwrap();
    fixture.pool.run_until_stalled();
    let channel = fixture.channel();
    assert_eq!(channel.borrow().close_signals, 1);
    assert!(matches!(channel.borrow().stopped, Some(Ok(()))));
    assert_eq!(fixture.calls(), 0);
    // Data-only decoding and owned-handle decoding refuse the stream.
    for refused in ["data", "owned"] {
        let message = fixture
            .root(async move |context, reference| {
                let result = match refused {
                    "data" => reference
                        .call_portable::<_, Value>(&context, "frames", (3_u32,))
                        .await
                        .map(drop),
                    _ => reference
                        .call_portable::<_, Owned>(&context, "frames", (3_u32,))
                        .await
                        .map(drop),
                };
                Ok(result.unwrap_err().message)
            })
            .unwrap();
        assert!(!message.is_empty(), "{refused}");
        fixture.pool.run_until_stalled();
        let channel = fixture.channel();
        assert_eq!(channel.borrow().close_signals, 1, "{refused}: {message}");
        assert!(channel.borrow().stopped.is_some(), "{refused}");
        assert_eq!(fixture.calls(), 0, "{refused}");
    }
    // Executor delivery carries the end, bound to the root and without a claim; dropping the
    // delivery is the close.
    let carried = fixture
        .root(async move |context, reference| {
            let delivered = reference
                .call_portable::<_, Delivered>(&context, "frames", (3_u32,))
                .await?;
            Ok((
                delivered.value.clone(),
                delivered.owned.len(),
                delivered.streams.len(),
            ))
        })
        .unwrap();
    assert_eq!(carried, (serde_json::json!(0), 0, 1));
    fixture.pool.run_until_stalled();
    let channel = fixture.channel();
    assert_eq!(channel.borrow().close_signals, 1, "delivered");
    assert!(channel.borrow().stopped.is_some(), "delivered");
    assert_eq!(fixture.calls(), 0, "delivered");
    fixture.close();
}

#[test]
fn a_lost_receiver_closes_the_head_once_and_the_producer_still_settles() {
    let mut fixture = fixture();
    let (entry, held) = gate();
    fixture.producer.plan.borrow_mut().entry = Some(held);
    let (abort, registration) = AbortHandle::new_pair();
    let caller = {
        let (host, reference) = (fixture.host.clone(), fixture.reference.clone());
        Abortable::new(
            async move {
                host.root("host", move |context| {
                    async move {
                        reference
                            .call_portable::<_, ByteStream>(&context, "frames", (3_u32,))
                            .await
                    }
                    .boxed_local()
                })
                .await
            },
            registration,
        )
    };
    let finished = Rc::new(Cell::new(false));
    {
        let finished = finished.clone();
        fixture
            .pool
            .spawner()
            .spawn_local(async move {
                let _ = caller.await;
                finished.set(true);
            })
            .unwrap();
    }
    fixture.pool.run_until_stalled();
    assert_eq!(fixture.calls(), 1, "the provider is parked before its head");
    // The caller and its root go away; the provider then publishes into nothing.
    abort.abort();
    fixture.pool.run_until_stalled();
    assert!(finished.get());
    let _ = entry.send(());
    fixture.pool.run_until_stalled();
    let channel = fixture.channel();
    assert_eq!(channel.borrow().close_signals, 1);
    assert!(channel.borrow().stopped.is_some());
    assert_eq!(fixture.calls(), 0);
    fixture.close();
}

#[test]
fn the_receiving_root_closes_a_retained_end_and_its_cancellation_fails_a_waiting_read() {
    let mut fixture = fixture();
    // A stream retained beyond its root is closed and unusable.
    let mut retained = fixture
        .root(async move |context, reference| {
            reference
                .call_portable::<_, ByteStream>(&context, "frames", (100_u32,))
                .await
        })
        .unwrap();
    assert_eq!(fixture.channel().borrow().close_signals, 1);
    let read = fixture.pool.run_until(retained.next()).unwrap();
    assert_eq!(read.unwrap_err().code, core::Code::Expired);
    drop(retained);
    fixture.pool.run_until_stalled();
    assert_eq!(fixture.channel().borrow().close_signals, 1, "closed once");
    assert_eq!(fixture.calls(), 0);
    // A read waiting for data wakes with the root's cancellation, not a clean end.
    let (begin, held) = gate();
    fixture.producer.plan.borrow_mut().begin = Some(held);
    let read = fixture
        .root(async move |context, reference| {
            let mut stream = reference
                .call_portable::<_, ByteStream>(&context, "frames", (3_u32,))
                .await?;
            let cancel = async {
                futures::future::ready(()).await;
                context.cancel()
            };
            let (read, cancelled) = futures::join!(stream.next(), cancel);
            cancelled?;
            drop(begin);
            Ok(read)
        })
        .unwrap();
    assert_eq!(read.unwrap().unwrap_err().code, core::Code::Cancelled);
    fixture.pool.run_until_stalled();
    assert_eq!(fixture.channel().borrow().close_signals, 1);
    assert_eq!(fixture.calls(), 0);
    fixture.close();
}

#[test]
fn host_close_waits_for_physical_work_after_the_head() {
    let mut fixture = fixture();
    let (finish, held) = gate();
    fixture.producer.plan.borrow_mut().finish = Some(held);
    fixture
        .root(async move |context, reference| {
            let stream = reference
                .call_portable::<_, ByteStream>(&context, "frames", (100_u32,))
                .await?;
            drop(stream);
            Ok(())
        })
        .unwrap();
    fixture.pool.run_until_stalled();
    assert_eq!(
        fixture.calls(),
        1,
        "the producer has not physically stopped"
    );
    fixture.reference.release().unwrap();
    let closed = Rc::new(Cell::new(false));
    {
        let (host, closed) = (fixture.host.clone(), closed.clone());
        fixture
            .pool
            .spawner()
            .spawn_local(async move {
                host.close().await.unwrap();
                closed.set(true);
            })
            .unwrap();
    }
    fixture.pool.run_until_stalled();
    assert!(!closed.get(), "close returned before physical settlement");
    let _ = finish.send(());
    fixture.pool.run_until_stalled();
    assert!(closed.get());
    let diagnostics = fixture.host.kernel().diagnostics();
    assert_eq!(
        (
            diagnostics.calls,
            diagnostics.references,
            diagnostics.roots,
            diagnostics.objects
        ),
        (0, 0, 0, 0)
    );
    assert!(fixture.host.errors().is_empty());
}

#[test]
fn a_failing_producer_fails_the_body_and_is_recorded_after_its_head() {
    let mut fixture = fixture();
    fixture.producer.plan.borrow_mut().fail = Some("producer trapped".into());
    let reads = fixture
        .root(async move |context, reference| {
            let mut stream = reference
                .call_portable::<_, ByteStream>(&context, "frames", (2_u32,))
                .await?;
            let mut reads = Vec::new();
            while let Some(read) = stream.next().await {
                reads.push(read.map(|chunk| chunk[0]).map_err(|error| error.message));
            }
            Ok(reads)
        })
        .unwrap();
    assert_eq!(
        reads,
        vec![Ok(0), Ok(1), Err("producer trapped".to_owned())],
        "a failed body never reads as a clean end"
    );
    fixture.pool.run_until_stalled();
    let errors = fixture.host.errors();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].message, "producer trapped");
    assert_eq!(fixture.calls(), 0);
    fixture.close();
}

#[test]
fn json_callers_refuse_a_stream_head_only_after_physical_settlement() {
    let mut fixture = fixture();
    // The producer also fails after the refusal closed its stream.
    fixture.producer.plan.borrow_mut().fail = Some("producer trapped".into());
    let host = fixture.host.clone();
    let producer = fixture.producer.clone();
    let stopped_at_return = fixture
        .root(async move |context, reference| {
            let call = host
                .kernel()
                .begin_call(reference.id(), context.root, "frames")?;
            let id = call.id;
            let result = host.invoke_portable(call, json!([100])).await;
            let stopped = producer
                .channel
                .borrow()
                .as_ref()
                .is_some_and(|channel| channel.borrow().stopped.is_some());
            host.kernel().finish_call(id)?;
            // The refusal stays the returned error.
            assert!(result.unwrap_err().message.contains("data-only boundary"));
            Ok(stopped)
        })
        .unwrap();
    assert!(
        stopped_at_return,
        "the refusal waited for the producer to stop"
    );
    assert_eq!(fixture.channel().borrow().close_signals, 1);
    // The physical failure behind the refusal is not lost; it is recorded once.
    let errors = fixture.host.errors();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].message, "producer trapped");
    // A data head followed by a failing physical step returns that failure, recorded nowhere else.
    let host = fixture.host.clone();
    let late = fixture
        .root(async move |context, reference| {
            let call = host
                .kernel()
                .begin_call(reference.id(), context.root, "late")?;
            let id = call.id;
            let result = host.invoke_portable(call, json!([])).await;
            host.kernel().finish_call(id)?;
            Ok(result)
        })
        .unwrap();
    assert_eq!(late.unwrap_err().message, "cleanup failed");
    assert!(fixture.host.errors().is_empty(), "not reported twice");
    // Scalar JSON calls are unchanged.
    let host = fixture.host.clone();
    let scalar = fixture
        .root(async move |context, reference| {
            let call = host
                .kernel()
                .begin_call(reference.id(), context.root, "scalar")?;
            let id = call.id;
            let result = host.invoke_portable(call, json!([41])).await;
            host.kernel().finish_call(id)?;
            result
        })
        .unwrap();
    assert_eq!(scalar, json!(42));
    fixture.close();
}

/// A consumer decoder that keeps the whole payload, stream end included.
struct Keep(#[allow(dead_code)] Payload);

impl FromPayload for Keep {
    fn from_payload(payload: Payload, _: &Origin) -> tembra::Result<Self> {
        Ok(Keep(payload))
    }
}

#[test]
fn a_custom_decoder_cannot_keep_a_stream_end_past_its_root() {
    let mut fixture = fixture();
    let kept = fixture
        .root(async move |context, reference| {
            reference
                .call_portable::<_, Keep>(&context, "frames", (100_u32,))
                .await
        })
        .unwrap();
    // The end was bound before the decoder saw it, so the root's end closed it.
    assert_eq!(fixture.channel().borrow().close_signals, 1);
    fixture.pool.run_until_stalled();
    assert!(fixture.channel().borrow().stopped.is_some());
    assert_eq!(fixture.calls(), 0);
    drop(kept);
    assert_eq!(fixture.channel().borrow().close_signals, 1, "closed once");
    fixture.close();
}

/// A consumer decoder that panics.
struct Boom;

impl FromPayload for Boom {
    fn from_payload(_: Payload, _: &Origin) -> tembra::Result<Self> {
        panic!("the consumer decoder panicked")
    }
}

#[test]
fn a_panicking_decoder_keeps_the_physical_work_and_its_invocation() {
    let mut fixture = fixture();
    let (finish, held) = gate();
    fixture.producer.plan.borrow_mut().finish = Some(held);
    let error = fixture
        .root(async move |context, reference| {
            reference
                .call_portable::<_, Boom>(&context, "frames", (100_u32,))
                .await
                .map(drop)
        })
        .unwrap_err();
    assert_eq!(error.message, "native result decoding panicked");
    fixture.pool.run_until_stalled();
    // The unwound head closed its stream; the gated producer still holds the invocation.
    assert_eq!(fixture.channel().borrow().close_signals, 1);
    assert_eq!(fixture.calls(), 1);
    let _ = finish.send(());
    fixture.pool.run_until_stalled();
    assert!(fixture.channel().borrow().stopped.is_some());
    assert_eq!(fixture.calls(), 0);
    fixture.close();
}

#[test]
fn a_resource_method_keeps_self_pinned_until_its_stream_work_settles() {
    let mut fixture = fixture();
    OBSERVED.with(|host| *host.borrow_mut() = Some(fixture.host.clone()));
    let (finish, held) = gate();
    fixture.producer.plan.borrow_mut().finish = Some(held);
    let log = fixture.producer.decoders.clone();
    let closed_while_streaming = log.clone();
    fixture
        .root(async move |context, reference| {
            let owned = reference
                .call_portable::<_, Owned>(&context, CONSTRUCTOR, ())
                .await?;
            let mut stream = owned
                .call_portable::<_, ByteStream>(&context, DECODE, ())
                .await?;
            // Dropping the handle after the head is not the close while its call still works.
            drop(owned);
            stream.next().await.unwrap()?;
            assert_eq!(closed_while_streaming.closed.get(), 0);
            Ok(())
        })
        .unwrap();
    fixture.pool.run_until_stalled();
    assert_eq!(
        log.closed.get(),
        0,
        "self stays pinned by the physical work"
    );
    assert_eq!(fixture.calls(), 1);
    let _ = finish.send(());
    fixture.pool.run_until_stalled();
    assert_eq!(log.closed.get(), 1, "closed exactly once");
    assert_eq!(
        log.calls_at_close.get(),
        Some(1),
        "closed before the invocation finished"
    );
    assert_eq!(fixture.calls(), 0);
    OBSERVED.with(|host| host.borrow_mut().take());
    fixture.close();
}
