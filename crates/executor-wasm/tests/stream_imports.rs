//! A compiled consumer Orb reading the `stream<u8>` result of an imported compiled producer Orb.
//!
//! The fixtures are real components built from `rust-stream-producer` and `rust-stream-consumer`
//! against `tests/wit/streams`, supplied as `TEMBRA_WASM_STREAM_PRODUCER` and
//! `TEMBRA_WASM_STREAM_CONSUMER`. Every case runs on Cranelift and on Pulley, through the real
//! host, kernel references and roots.
//!
//! The producer's call keeps running under its own invocation after its head. The consumer's
//! callback pumps the delivered end into a bounded pipe of its own Store, which its guest reads.
//! A plain `stream<u8>` carries no error: a consumer guest sees a producer failure as the end of
//! the stream, while the host records the failure.
use futures::{FutureExt, channel::oneshot, executor::LocalPool, task::LocalSpawnExt};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::{Duration, Instant},
};
use tembra::{
    Host, Orb, Reference, Service, core,
    portable::{ByteSource, ByteStream, Outcome, Payload},
};
use tembra_executor_wasm::{Contract, Engine, Install, Limits};

const FRAMES: &str = "tembra-test:streams/frames@1.0.0";
const PRODUCER: &str = "frames::tembra-test:streams/frames@1.0.0";
const CONSUMER: &str = "consumer::tembra-test:streams/digest@1.0.0";

/// Producer modes, as documented in `streams.wit`.
const CLEAN: u32 = 0;
const TRAP_HOLDING: u32 = 1;
const TRAP_AFTER_END: u32 = 2;
const WAIT_FOR_READER: u32 = 3;
const IDLE: u32 = 4;

fn fixture(variable: &str) -> Option<Vec<u8>> {
    let Some(path) = std::env::var_os(variable).filter(|path| !path.is_empty()) else {
        eprintln!("skipped: {variable} names no compiled stream fixture");
        return None;
    };
    Some(std::fs::read(path).unwrap())
}

fn contract() -> Contract {
    Contract::from_dir(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/wit/streams"),
        Some("producer"),
    )
    .unwrap()
}

fn consumer_contract() -> Contract {
    Contract::from_dir(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/wit/streams"),
        Some("consumer"),
    )
    .unwrap()
}

fn engine(interpreted: bool, timeout_ms: u64) -> Engine {
    // A checksum over every byte of a 16 MiB stream, on both sides, needs more than the
    // default fuel.
    let limits = Limits {
        timeout_ms,
        fuel: 20_000_000_000,
        ..Limits::default()
    };
    if interpreted {
        Engine::interpreted(limits)
    } else {
        Engine::new(limits)
    }
    .unwrap()
}

fn producer(engine: &Engine, bytes: &[u8]) -> Orb {
    engine
        .load(
            &contract()
                .component_artifact(bytes, "frames", 1, &Default::default())
                .unwrap(),
            Install::default(),
        )
        .unwrap()
}

fn consumer(engine: &Engine, bytes: &[u8]) -> Orb {
    engine
        .load(
            &consumer_contract()
                .component_artifact(bytes, "consumer", 1, &Default::default())
                .unwrap(),
            Install::default().requires(FRAMES, "frames", 1),
        )
        .unwrap()
}

/// Run `body` with both fixtures once on Cranelift and once on Pulley.
fn engines(body: impl Fn(&[u8], &[u8], bool)) {
    let (Some(producer), Some(consumer)) = (
        fixture("TEMBRA_WASM_STREAM_PRODUCER"),
        fixture("TEMBRA_WASM_STREAM_CONSUMER"),
    ) else {
        return;
    };
    for interpreted in [false, true] {
        body(&producer, &consumer, interpreted);
    }
}

struct Pair {
    pool: LocalPool,
    host: Host,
    engine: Engine,
    consumer: Rc<Reference>,
    producer: Rc<Reference>,
}

/// Install a producer Orb and its consumer; `frames` may replace the compiled producer.
fn pair(engine: Engine, frames: Orb, consumer_bytes: &[u8]) -> Pair {
    let pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![frames, consumer(&engine, consumer_bytes)])
        .unwrap();
    let consumer = Rc::new(host.get(CONSUMER, vec!["main".into()], "host").unwrap());
    let producer = Rc::new(host.get(PRODUCER, vec!["main".into()], "host").unwrap());
    let mut pair = Pair {
        pool,
        host,
        engine,
        consumer,
        producer,
    };
    let (consumer, producer) = (pair.consumer.clone(), pair.producer.clone());
    pair.pool.run_until(consumer.ready()).unwrap();
    pair.pool.run_until(producer.ready()).unwrap();
    pair
}

fn compiled(
    producer_bytes: &[u8],
    consumer_bytes: &[u8],
    interpreted: bool,
    timeout_ms: u64,
) -> Pair {
    let engine = engine(interpreted, timeout_ms);
    let frames = producer(&engine, producer_bytes);
    pair(engine, frames, consumer_bytes)
}

impl Pair {
    /// Call the consumer as `alice`, in a fresh root.
    fn call<A: serde::Serialize + 'static, R: serde::de::DeserializeOwned + 'static>(
        &mut self,
        method: &'static str,
        arguments: A,
    ) -> tembra::Result<R> {
        let reference = self.consumer.clone();
        self.pool.run_until(self.host.root("alice", move |context| {
            async move { reference.call_portable(&context, method, arguments).await }.boxed_local()
        }))
    }

    fn sum(&mut self, count: u32, size: u32, mode: u32, take: u64) -> tembra::Result<(u64, u64)> {
        self.call("sum", (count, size, mode, take))
    }

    /// The producer's own scalar call.
    fn producer_ping(&mut self) -> tembra::Result<u32> {
        let reference = self.producer.clone();
        self.pool.run_until(self.host.root("alice", move |context| {
            async move { reference.call_portable(&context, "ping", (1_u32,)).await }.boxed_local()
        }))
    }

    fn calls(&self) -> usize {
        self.host.kernel().diagnostics().calls
    }

    /// Let the Stores' threads work until `done` holds, polling this host meanwhile.
    fn wait(&mut self, mut done: impl FnMut(&Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !done(self) {
            assert!(Instant::now() < deadline, "the condition never held");
            self.pool.run_until_stalled();
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Both Orbs settled; the consumer still serves.
    fn settled(&mut self) {
        self.wait(|pair| pair.calls() == 0);
        assert_eq!(self.call::<_, u32>("ping", (41_u32,)).unwrap(), 42);
    }

    /// Close the host; every kernel record and Store must be gone.
    fn close(mut self) {
        self.consumer.release().unwrap();
        self.producer.release().unwrap();
        self.pool.run_until(self.host.close()).unwrap();
        let diagnostics = self.host.kernel().diagnostics();
        assert_eq!(
            (diagnostics.calls, diagnostics.references, diagnostics.roots),
            (0, 0, 0)
        );
        assert_eq!(self.engine.instances(), 0);
    }
}

/// Byte `j` of chunk `i` is `(i + j) mod 256`; the consumer's order-sensitive checksum.
fn checksum(count: u32, size: u32, take: u64) -> (u64, u64) {
    let bytes = (0..count)
        .flat_map(|chunk| (0..size).map(move |offset| ((chunk + offset) % 256) as u8))
        .take(take.min(u64::from(count) * u64::from(size)) as usize);
    let (mut length, mut sum) = (0_u64, 0_u64);
    for byte in bytes {
        length += 1;
        sum = sum.wrapping_mul(31).wrapping_add(u64::from(byte));
    }
    (length, sum)
}

fn quarantined(error: &tembra::Error) -> bool {
    error.reason == "wasm_executor_quarantined"
}

#[test]
#[ignore = "requires TEMBRA_WASM_STREAM_PRODUCER and TEMBRA_WASM_STREAM_CONSUMER"]
fn a_consumer_reads_the_whole_stream_of_its_import() {
    engines(|producer, consumer, interpreted| {
        let mut pair = compiled(producer, consumer, interpreted, 30_000);
        assert_eq!(
            pair.sum(256, 4096, CLEAN, u64::MAX).unwrap(),
            checksum(256, 4096, u64::MAX),
            "{interpreted}"
        );
        // 16 MiB in 4 KiB reads: every read wakes the pump across threads, and the pump wakes
        // the producer, through two 64 KiB pipes.
        assert_eq!(
            pair.sum(1024, 16 << 10, CLEAN, u64::MAX).unwrap(),
            checksum(1024, 16 << 10, u64::MAX),
            "{interpreted}"
        );
        pair.settled();
        assert_eq!(pair.producer_ping().unwrap(), 2);
        assert!(pair.host.errors().is_empty());
        pair.close();
    });
}

#[test]
#[ignore = "requires TEMBRA_WASM_STREAM_PRODUCER and TEMBRA_WASM_STREAM_CONSUMER"]
fn stopping_early_or_abandoning_the_import_lets_the_producer_finish() {
    engines(|producer, consumer, interpreted| {
        let mut pair = compiled(producer, consumer, interpreted, 30_000);
        // The consumer reads 8 KiB and drops its reader; the producer sees it at its next write.
        assert_eq!(
            pair.sum(1000, 4096, CLEAN, 8192).unwrap(),
            checksum(1000, 4096, 8192),
            "{interpreted}"
        );
        pair.settled();
        // The guest polls the import once and drops it: the reply guard closes the pipe and the
        // late end closes once.
        let _: bool = pair.call("abandon", (1000_u32, 4096_u32)).unwrap();
        pair.settled();
        assert_eq!(pair.producer_ping().unwrap(), 2, "{interpreted}");
        assert!(pair.host.errors().is_empty(), "consumer closes are healthy");
        pair.close();
    });
}

#[test]
#[ignore = "requires TEMBRA_WASM_STREAM_PRODUCER and TEMBRA_WASM_STREAM_CONSUMER"]
fn a_producer_failure_ends_the_consumer_stream_and_is_recorded() {
    engines(|producer, consumer, interpreted| {
        for mode in [TRAP_HOLDING, TRAP_AFTER_END] {
            let mut pair = compiled(producer, consumer, interpreted, 30_000);
            // A plain byte stream has no error: the consumer sees its end after a prefix of the
            // data. A recorded failure ends the delivered end at once, before bytes it had not
            // yet handed on, so the prefix may be anything up to the whole stream.
            // Exceed the producer pipe: the trap cannot happen until its head was delivered
            // and the importer started draining. A tiny body could trap before that head,
            // which legitimately fails the import itself instead of an already-open stream.
            let (read, sum) = pair.sum(128, 4096, mode, u64::MAX).unwrap();
            assert!(read <= 128 * 4096, "{interpreted} {mode}: {read}");
            assert_eq!(
                (read, sum),
                checksum(128, 4096, read),
                "{interpreted} {mode}"
            );
            pair.settled();
            let errors = pair.host.errors();
            assert!(
                errors.iter().any(|error| error.message.contains("trap")),
                "{interpreted} {mode}: {errors:?}"
            );
            assert!(quarantined(&pair.producer_ping().unwrap_err()));
            pair.close();
        }
    });
}

#[test]
#[ignore = "requires TEMBRA_WASM_STREAM_PRODUCER and TEMBRA_WASM_STREAM_CONSUMER"]
fn a_consumer_failure_closes_the_stream_and_the_producer_stays_healthy() {
    engines(|producer, consumer, interpreted| {
        let mut pair = compiled(producer, consumer, interpreted, 30_000);
        let error = pair
            .call::<_, u32>("trap", (1000_u32, 4096_u32, 8192_u64))
            .unwrap_err();
        assert!(error.message.contains("trap"), "{interpreted}: {error}");
        pair.wait(|pair| pair.calls() == 0);
        assert!(quarantined(
            &pair.call::<_, u32>("ping", (1_u32,)).unwrap_err()
        ));
        assert_eq!(pair.producer_ping().unwrap(), 2, "{interpreted}");
        pair.close();
    });
}

#[test]
#[ignore = "requires TEMBRA_WASM_STREAM_PRODUCER and TEMBRA_WASM_STREAM_CONSUMER"]
fn cancelling_the_root_ends_the_import_and_both_orbs_settle() {
    engines(|producer, consumer, interpreted| {
        let mut pair = compiled(producer, consumer, interpreted, 30_000);
        let reference = pair.consumer.clone();
        // The producer writes nothing and waits for its reader; the consumer's read waits.
        let started = Instant::now();
        let outcome = pair.pool.run_until(pair.host.root("alice", move |context| {
            async move {
                let sum = reference.call_portable::<_, (u64, u64)>(
                    &context,
                    "sum",
                    (0_u32, 0_u32, WAIT_FOR_READER, u64::MAX),
                );
                let cancel = async {
                    futures_timer::Delay::new(Duration::from_millis(200)).await;
                    context.cancel()
                };
                let (sum, cancelled) = futures::join!(sum, cancel);
                cancelled?;
                Ok(sum)
            }
            .boxed_local()
        }));
        // The root's cancellation cancels the delivered end and revokes the consumer callback's
        // epoch; either ends its pump, so the guest's read sees the writer dropped without data.
        // A compiled callback stops cooperatively: it may still return what it read.
        match outcome.unwrap() {
            Ok(read) => assert_eq!(read, (0, 0), "{interpreted}"),
            Err(error) => assert_eq!(error.code, core::Code::Cancelled, "{interpreted}: {error}"),
        }
        assert!(started.elapsed() < Duration::from_secs(5), "{interpreted}");
        pair.settled();
        assert_eq!(pair.producer_ping().unwrap(), 2, "{interpreted}");
        pair.close();
    });
}

#[test]
#[ignore = "requires TEMBRA_WASM_STREAM_PRODUCER and TEMBRA_WASM_STREAM_CONSUMER"]
fn an_idle_producer_ends_both_calls_at_their_terminal_timeout() {
    engines(|producer, consumer, interpreted| {
        let mut pair = compiled(producer, consumer, interpreted, 1000);
        let started = Instant::now();
        let error = pair.sum(0, 0, IDLE, u64::MAX).unwrap_err();
        assert!(
            started.elapsed() >= Duration::from_millis(900),
            "{interpreted}"
        );
        assert!(
            error.message.contains("timed out"),
            "{interpreted}: {error}"
        );
        pair.wait(|pair| pair.calls() == 0);
        assert!(quarantined(
            &pair.call::<_, u32>("ping", (1_u32,)).unwrap_err()
        ));
        assert!(quarantined(&pair.producer_ping().unwrap_err()));
        pair.pool.run_until(pair.host.settled());
        assert_eq!(pair.engine.instances(), 0, "{interpreted}");
        let errors = pair.host.errors();
        // The scalar consumer result does not expose whether its upstream stream was delivered.
        // A pre-head failure is returned directly; a published producer reports its late failure
        // once. The direct-stream tests separately require that exact report after an observed head.
        assert!(
            errors.len() <= 1,
            "{interpreted}: duplicate timeout: {errors:?}"
        );
        for late in errors {
            assert_eq!(late, error, "{interpreted}: unexpected late failure");
        }
        pair.close();
    });
}

#[test]
#[ignore = "requires TEMBRA_WASM_STREAM_PRODUCER and TEMBRA_WASM_STREAM_CONSUMER"]
fn a_reader_kept_past_its_callback_is_ended() {
    engines(|producer, consumer, interpreted| {
        let mut pair = compiled(producer, consumer, interpreted, 30_000);
        let _: u32 = pair.call("retain", (1000_u32, 4096_u32)).unwrap();
        // The callback that received the stream ended, so its pump ended it.
        pair.wait(|pair| pair.calls() == 0);
        let read: u64 = pair.call("retained", [0_u32; 0]).unwrap();
        assert_eq!(read, 0, "{interpreted}");
        pair.settled();
        assert_eq!(pair.producer_ping().unwrap(), 2);
        pair.close();
    });
}

#[test]
#[ignore = "requires TEMBRA_WASM_STREAM_PRODUCER and TEMBRA_WASM_STREAM_CONSUMER"]
fn host_close_and_producer_reload_wait_for_the_stream() {
    engines(|producer_bytes, consumer_bytes, interpreted| {
        // Close while a long stream is being read.
        let mut pair = compiled(producer_bytes, consumer_bytes, interpreted, 30_000);
        let started = Rc::new(Cell::new(false));
        {
            let (host, reference, started) =
                (pair.host.clone(), pair.consumer.clone(), started.clone());
            pair.pool
                .spawner()
                .spawn_local(async move {
                    let _ = host
                        .root("alice", move |context| {
                            started.set(true);
                            async move {
                                reference
                                    .call_portable::<_, (u64, u64)>(
                                        &context,
                                        "sum",
                                        (1_000_000_u32, 4096_u32, CLEAN, u64::MAX),
                                    )
                                    .await
                            }
                            .boxed_local()
                        })
                        .await;
                })
                .unwrap();
        }
        pair.wait(|pair| started.get() && pair.calls() == 2);
        pair.close();

        // Reload the producer mid-stream: the stream finishes from its old Store.
        let mut pair = compiled(producer_bytes, consumer_bytes, interpreted, 30_000);
        let reload = Rc::new(RefCell::new(None));
        let sum = {
            let (engine, bytes, reload) =
                (pair.engine.clone(), producer_bytes.to_vec(), reload.clone());
            let (host, reference) = (pair.host.clone(), pair.consumer.clone());
            pair.pool
                .run_until(host.clone().root("alice", move |context| {
                    async move {
                        let sum = reference.call_portable::<_, (u64, u64)>(
                            &context,
                            "sum",
                            (256_u32, 16384_u32, CLEAN, u64::MAX),
                        );
                        let replace = async {
                            futures_timer::Delay::new(Duration::from_millis(20)).await;
                            *reload.borrow_mut() =
                                Some(host.reload(vec![producer(&engine, &bytes)], 30_000).await);
                        };
                        let (sum, ()) = futures::join!(sum, replace);
                        sum
                    }
                    .boxed_local()
                }))
        };
        assert_eq!(
            sum.unwrap(),
            checksum(256, 16384, u64::MAX),
            "{interpreted}"
        );
        reload.borrow_mut().take().unwrap().unwrap();
        pair.settled();
        assert_eq!(pair.producer_ping().unwrap(), 2);
        pair.close();
    });
}

/// A native `frames` provider: every call answers at once with a stream of ready chunks, so
/// many heads can be outstanding together. `reenter` makes it call back into the consumer.
fn native_frames(reenter: Rc<RefCell<Option<Rc<Reference>>>>) -> Orb {
    let service = contract()
        .plan("frames", 1, &Default::default())
        .unwrap()
        .orb
        .services
        .remove(0);
    Orb::new("frames", 1).service(
        Service::new(service.name, |_| Ok(()))
            .signature(service.signature)
            .payload_method("frames", move |_, context, payload: Payload| {
                let reenter = reenter.borrow().clone();
                async move {
                    if let Some(consumer) = reenter {
                        consumer
                            .call_portable::<_, u32>(&context, "ping", (1_u32,))
                            .await?;
                    }
                    let arguments = payload.into_data()?;
                    let (count, size) = (
                        arguments[0].as_u64().unwrap(),
                        arguments[1].as_u64().unwrap(),
                    );
                    let mut head = Payload::data(Value::Null);
                    let index = head.attach_stream(ByteStream::new(Ready {
                        chunks: (0..count)
                            .map(|chunk| {
                                (0..size)
                                    .map(|offset| ((chunk + offset) % 256) as u8)
                                    .collect()
                            })
                            .collect(),
                    }));
                    let head = head.map_value(|_| Ok(json!(index)));
                    Ok(Outcome::continuing(head, async { Ok(()) }))
                }
            })
            .method(
                "ping",
                |_, _, (value,): (u32,)| async move { Ok(value + 1) },
            ),
    )
}

/// Chunks that are all available at once.
struct Ready {
    chunks: std::collections::VecDeque<Vec<u8>>,
}

impl ByteSource for Ready {
    fn poll_next(
        &mut self,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<tembra::Result<Vec<u8>>>> {
        std::task::Poll::Ready(self.chunks.pop_front().map(Ok))
    }

    fn close(&mut self) {
        self.chunks.clear();
    }

    fn poll_closed(
        &mut self,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<tembra::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

#[test]
#[ignore = "requires TEMBRA_WASM_STREAM_CONSUMER"]
fn outstanding_stream_imports_per_callback_are_bounded() {
    let Some(consumer) = fixture("TEMBRA_WASM_STREAM_CONSUMER") else {
        return;
    };
    for interpreted in [false, true] {
        let engine = engine(interpreted, 30_000);
        let mut pair = pair(engine, native_frames(Rc::default()), &consumer);
        // Sixteen heads at once, each then read to its end. Every stream is larger than its
        // 64 KiB pipe, so each pump stays outstanding until the guest reads that stream.
        let total: u64 = pair.call("many", (16_u32, 20_u32, 4096_u32)).unwrap();
        assert_eq!(total, 16 * 20 * 4096, "{interpreted}");
        pair.settled();
        // A seventeenth outstanding stream import is refused, which traps the importer.
        let error = pair
            .call::<_, u64>("many", (17_u32, 20_u32, 4096_u32))
            .unwrap_err();
        assert!(
            error.message.contains("stream imports outstanding"),
            "{interpreted}: {error}"
        );
        pair.wait(|pair| pair.calls() == 0);
        let _ = pair.host.errors();
        pair.close();
    }
}

#[test]
#[ignore = "requires TEMBRA_WASM_STREAM_CONSUMER"]
fn a_provider_calling_back_into_its_streaming_importer_is_refused() {
    let Some(consumer) = fixture("TEMBRA_WASM_STREAM_CONSUMER") else {
        return;
    };
    for interpreted in [false, true] {
        let engine = engine(interpreted, 5000);
        let back = Rc::new(RefCell::new(None));
        let mut pair = pair(engine, native_frames(back.clone()), &consumer);
        *back.borrow_mut() = Some(pair.consumer.clone());
        let started = Instant::now();
        let error = pair.sum(4, 1024, CLEAN, u64::MAX).unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{interpreted}: reentry waited instead of being refused"
        );
        assert!(
            error.message.contains("causal reentry"),
            "{interpreted}: {error}"
        );
        back.borrow_mut().take();
        pair.wait(|pair| pair.calls() == 0);
        pair.close();
    }
}

/// What a quiet provider's stream reports: how often it was closed, and the two parties waiting
/// for the first close.
#[derive(Default)]
struct Quiet {
    closes: Rc<Cell<usize>>,
    /// The provider's `ping` and the call's physical work each wait for one of these.
    waiting: Option<(oneshot::Sender<()>, oneshot::Sender<()>)>,
}

impl ByteSource for Quiet {
    /// Never any data, and nothing will ever wake a read: only a close ends this stream.
    fn poll_next(
        &mut self,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<tembra::Result<Vec<u8>>>> {
        std::task::Poll::Pending
    }

    fn close(&mut self) {
        self.closes.set(self.closes.get() + 1);
        if let Some((ping, physical)) = self.waiting.take() {
            let _ = ping.send(());
            let _ = physical.send(());
        }
    }

    fn poll_closed(
        &mut self,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<tembra::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

fn quiet(message: &str) -> tembra::Error {
    tembra::Error::new(core::Code::Unavailable, "quiet_frames", message)
}

/// A native `frames` provider whose stream stays quiet until it is closed. Its `ping` answers
/// only once that close was issued, and its `frames` call physically ends only then.
fn quiet_frames(closes: Rc<Cell<usize>>) -> Orb {
    let service = contract()
        .plan("frames", 1, &Default::default())
        .unwrap()
        .orb
        .services
        .remove(0);
    let closed = Rc::new(RefCell::new(None::<oneshot::Receiver<()>>));
    let pinged = closed.clone();
    Orb::new("frames", 1).service(
        Service::new(service.name, |_| Ok(()))
            .signature(service.signature)
            .payload_method("frames", move |_, _, _: Payload| {
                let (ping, ping_waits) = oneshot::channel();
                let (physical, physical_waits) = oneshot::channel();
                *closed.borrow_mut() = Some(ping_waits);
                let mut head = Payload::data(Value::Null);
                let index = head.attach_stream(ByteStream::new(Quiet {
                    closes: closes.clone(),
                    waiting: Some((ping, physical)),
                }));
                let head = head.map_value(|_| Ok(json!(index)));
                async move {
                    Ok(Outcome::continuing(head, async move {
                        physical_waits
                            .await
                            .map_err(|_| quiet("the stream was never closed"))
                    }))
                }
            })
            .method("ping", move |_, _, (value,): (u32,)| {
                let closed = pinged.borrow_mut().take();
                async move {
                    closed
                        .ok_or_else(|| quiet("ping before frames"))?
                        .await
                        .map_err(|_| quiet("the stream was never closed"))?;
                    Ok(value + 1)
                }
            }),
    )
}

#[test]
#[ignore = "requires TEMBRA_WASM_STREAM_CONSUMER"]
fn a_dropped_reader_closes_its_quiet_import_while_its_callback_goes_on() {
    let Some(consumer) = fixture("TEMBRA_WASM_STREAM_CONSUMER") else {
        return;
    };
    for interpreted in [false, true] {
        let engine = engine(interpreted, 5000);
        let closes = Rc::new(Cell::new(0));
        let mut pair = pair(engine, quiet_frames(closes.clone()), &consumer);
        let started = Instant::now();
        // The guest drops its reader and then waits in the imported `ping`, which answers only
        // after the stream's close. The callback does not end before that answer, so only the
        // pump's own view of the closed reader can issue the close.
        let pinged: u32 = pair
            .call("drop-then-ping", (4_u32, 1024_u32))
            .unwrap_or_else(|error| panic!("{interpreted}: {error}"));
        assert_eq!(pinged, 2, "{interpreted}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{interpreted}: the close waited for a timeout"
        );
        assert_eq!(closes.get(), 1, "{interpreted}");
        pair.settled();
        assert!(pair.host.errors().is_empty(), "{interpreted}");
        pair.close();
        assert_eq!(closes.get(), 1, "{interpreted}");
    }
}
