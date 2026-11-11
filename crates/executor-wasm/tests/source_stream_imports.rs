//! A source (TypeScript) Orb consuming the whole `stream<u8>` result of an imported Orb.
//!
//! The consumer is the SDK-authored fixture `tests/source/stream-reader` (reload replacement
//! `stream-reader-v2`), prepared by the production SDK build into `TEMBRA_WASM_FIXTURES` and
//! loaded through `Engine::load` (the native facade). Its producer is a native Orb, the source
//! Orb `tests/source/stream-producer`, or the compiled S2 producer
//! (`TEMBRA_WASM_STREAM_PRODUCER`); `tests/source/stream-relay` forwards an imported stream as its
//! own whole result. Every case runs on Cranelift and on Pulley, through the real host, kernel
//! references and roots.
//!
//! The consumer imports no WASI interface and receives no grant. Each import is the typed WIT
//! import of the selected dependency: the SDK selects the host-issued reference and calls the
//! import in the same turn, and the host pumps the delivered end into a bounded pipe of the
//! consumer's Store within its callback.
//!
//! Declared breaking semantics of the component runtime (COORDINATION, correction 5, and the
//! executor handoff, defect 1): a dependency call has no WIT error lane here, so a failed import
//! (a refused or failed head, a refused reentry, an exceeded bound) traps the importing callback
//! and quarantines the consumer revision. A producer failure after its head is never a clean
//! end either: a plain `stream<u8>` has no business-error lane, but the host owns the pump and
//! fails the importer's next read (`Pipe::fail_producer`), so the importing callback traps at
//! its physical drain and is quarantined. It never returns a successful truncated result, and a
//! relay never ends its own stream cleanly after a truncated import. Only a producer that itself
//! trapped (source or compiled) is quarantined; a native producer stays healthy. A WIT `result`
//! business error is separate and stays recoverable.
use futures::{FutureExt, channel::oneshot, executor::LocalPool, task::LocalSpawnExt};
#[path = "support/source.rs"]
mod source;
use serde_json::{Value, json};
use source::fixture;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tembra::{
    Host, Orb, Reference, Service, core,
    portable::{ByteSource, ByteStream, Outcome, Payload},
};
use tembra_executor_wasm::{Contract, Engine, Install, Limits};

const FRAMES: &str = "tembra-test:streams/frames@1.0.0";
const ALIASED: &str = "tembra-test:streams/aliased@1.0.0";
const READER: &str = "reader::tembra-test:streams/reader@1.0.0";
const RELAY: &str = "relay::tembra-test:streams/relayed@1.0.0";
const PRODUCER: &str = "frames::tembra-test:streams/frames@1.0.0";
/// The pipe's byte capacity of the component host.
const PIPE_BYTES: usize = 64 << 10;

/// Producer modes. The compiled S2 producer implements `CLEAN` and traps after its whole body
/// while holding its writer for `FAIL_AFTER`; the native and source producers implement all.
const CLEAN: u32 = 0;
/// Fail after half of the chunks.
const FAIL_AFTER: u32 = 1;
/// One chunk, then nothing until the stream is closed.
const QUIET: u32 = 4;
/// Fail before the head.
const FAIL_BEFORE: u32 = 5;
/// Native only: wait for a physical gate (noting a cancellation seen first), then answer.
const GATED: u32 = 6;
/// Source only: every chunk, then `throw undefined`.
const THROW_UNDEFINED: u32 = 7;
/// Native only: call back into the consumer first.
const REENTER: u32 = 8;

/// Consumer read styles (`reader.read`); `+ 10` reads the aliased import instead.
const CHUNKS: u32 = 0;
const BYTES: u32 = 1;
/// Read one chunk, then ask the producer what it produced (`ping(PROBE)`), then read the rest.
const PROBED: u32 = 2;
const ALIAS: u32 = 10;

/// Native `ping` arguments with a side effect instead of an answer.
const PROBE: u32 = 1000;
const STARTED: u32 = 2000;
const CLOSED: u32 = 3000;

/// The compiled S2 producer; its tests require it.
fn compiled_producer() -> Vec<u8> {
    let path = std::env::var_os("TEMBRA_WASM_STREAM_PRODUCER")
        .filter(|path| !path.is_empty())
        .expect("TEMBRA_WASM_STREAM_PRODUCER names the compiled stream producer");
    std::fs::read(path).unwrap()
}

/// The consumer's contract, plus provider worlds for the native producers' signatures.
fn contract(world: &str) -> Contract {
    let wit = format!(
        "{}\nworld producer-orb {{ export frames; }}\nworld aliased-orb {{ export aliased; }}\n",
        include_str!("source/stream-reader/contract.wit")
    );
    Contract::from_source("contract.wit", &wit, Some(world)).unwrap()
}

/// Byte `j` of chunk `i` is `(i + j) mod 256`.
fn exact(received: &[u8], size: usize) -> bool {
    received
        .iter()
        .enumerate()
        .all(|(offset, byte)| usize::from(*byte) == (offset / size + offset % size) % 256)
}

/// What the native producer saw and did.
#[derive(Default)]
struct Native {
    log: RefCell<Vec<String>>,
    /// `(principal, root)` of every `frames` call.
    calls: RefCell<Vec<(String, core::Id)>>,
    /// Ordinary `ping` calls.
    pings: Cell<usize>,
    /// Stream ends closed (each at most once).
    closes: Rc<Cell<usize>>,
    /// Chunks produced, across streams.
    produced: Rc<Cell<usize>>,
    /// Physical ends of `frames` calls.
    ended: Rc<Cell<usize>>,
    /// Chunks produced when the consumer probed.
    probed: Cell<Option<usize>>,
    /// Held by a probe, if set.
    probe_gate: RefCell<Option<oneshot::Receiver<()>>>,
    /// A `GATED` call is running.
    started: Cell<bool>,
    /// Held by a `GATED` call after its cancellation: its physical end.
    physical: RefCell<Option<oneshot::Receiver<()>>>,
    /// Called back by `REENTER`.
    reenter: RefCell<Option<Rc<Reference>>>,
}

impl Native {
    fn note(&self, entry: impl Into<String>) {
        self.log.borrow_mut().push(entry.into());
    }

    fn position(&self, entry: &str) -> usize {
        let log = self.log.borrow();
        log.iter()
            .position(|line| line == entry)
            .unwrap_or_else(|| panic!("{entry} not logged: {log:?}"))
    }
}

/// Wait in a native callback until `done` holds.
async fn until(mut done: impl FnMut() -> bool) {
    while !done() {
        futures_timer::Delay::new(Duration::from_millis(1)).await;
    }
}

fn unavailable(message: &str) -> tembra::Error {
    tembra::Error::new(core::Code::Unavailable, "native_frames", message)
}

/// Native chunks produced on demand; `fail_at` fails instead of chunk `fail_at`, `quiet_after`
/// stays pending (until closed) instead of chunk `quiet_after`. `done` ends the call physically.
struct Lazy {
    next: u32,
    count: u32,
    size: u32,
    fail_at: Option<u32>,
    quiet_after: Option<u32>,
    native: Rc<Native>,
    closed: bool,
    done: Option<oneshot::Sender<()>>,
}

impl ByteSource for Lazy {
    fn poll_next(
        &mut self,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<tembra::Result<Vec<u8>>>> {
        use std::task::Poll;
        if self.closed {
            return Poll::Ready(None);
        }
        if self.fail_at == Some(self.next) {
            self.done.take().map(|done| done.send(()));
            return Poll::Ready(Some(Err(unavailable(
                "native producer failed after its head",
            ))));
        }
        if self.quiet_after == Some(self.next) {
            return Poll::Pending;
        }
        if self.next == self.count {
            self.done.take().map(|done| done.send(()));
            return Poll::Ready(None);
        }
        let chunk = (0..self.size)
            .map(|offset| ((self.next + offset) % 256) as u8)
            .collect();
        self.next += 1;
        self.native.produced.set(self.native.produced.get() + 1);
        Poll::Ready(Some(Ok(chunk)))
    }

    fn close(&mut self) {
        if !self.closed {
            self.closed = true;
            self.native.closes.set(self.native.closes.get() + 1);
        }
        self.done.take().map(|done| done.send(()));
    }

    fn poll_closed(
        &mut self,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<tembra::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

/// A native producer Orb named `orb` exporting the `world`'s interface.
fn native(native: Rc<Native>, orb: &str, world: &str) -> Orb {
    let service = contract(world)
        .plan(orb, 1, &Default::default())
        .unwrap()
        .orb
        .services
        .remove(0);
    let pinged = native.clone();
    Orb::new(orb, 1).service(
        Service::new(service.name, |_| Ok(()))
            .signature(service.signature)
            .payload_method("frames", move |_, context, payload: Payload| {
                let native = native.clone();
                async move {
                    let arguments = payload.into_data()?;
                    let [count, size, mode] = [0, 1, 2].map(|at| arguments[at].as_u64().unwrap());
                    let (count, size, mode) = (count as u32, size as u32, mode as u32);
                    native
                        .calls
                        .borrow_mut()
                        .push((context.principal()?, context.root));
                    let reenter = native.reenter.borrow().clone();
                    match (mode, reenter) {
                        (FAIL_BEFORE, _) => {
                            return Err(unavailable("native producer refused before its head"));
                        }
                        (REENTER, Some(consumer)) => {
                            consumer
                                .call_portable::<_, u32>(&context, "ping", (1_u32,))
                                .await?;
                        }
                        (GATED, _) => {
                            native.note("gated:start");
                            native.started.set(true);
                            // Its physical gate, or its own cancellation and then the gate.
                            let physical = native.physical.borrow_mut().take();
                            let cancelled = context.signal();
                            let cancelled = std::pin::pin!(cancelled.cancelled());
                            if let Some(physical) = physical {
                                match futures::future::select(cancelled, physical).await {
                                    futures::future::Either::Left((_, physical)) => {
                                        native.note("gated:signalled");
                                        let _ = physical.await;
                                    }
                                    futures::future::Either::Right(_) => {}
                                }
                            } else {
                                cancelled.await;
                                native.note("gated:signalled");
                            }
                            native.note("gated:end");
                        }
                        _ => {}
                    }
                    let (done, finished) = oneshot::channel();
                    let mut head = Payload::data(Value::Null);
                    let index = head.attach_stream(ByteStream::new(Lazy {
                        next: 0,
                        count,
                        size,
                        fail_at: (mode == FAIL_AFTER).then_some(count / 2),
                        quiet_after: matches!(mode, QUIET | GATED).then_some(1),
                        native: native.clone(),
                        closed: false,
                        done: Some(done),
                    }));
                    let head = head.map_value(|_| Ok(json!(index)));
                    let ended = native.ended.clone();
                    Ok(Outcome::continuing(head, async move {
                        let _ = finished.await;
                        ended.set(ended.get() + 1);
                        Ok(())
                    }))
                }
            })
            .method("ping", move |_, _, (value,): (u32,)| {
                let native = pinged.clone();
                async move {
                    match value {
                        PROBE => {
                            // Long enough for an unbounded producer to run far ahead.
                            futures_timer::Delay::new(Duration::from_millis(100)).await;
                            native.probed.set(Some(native.produced.get()));
                            let gate = native.probe_gate.borrow_mut().take();
                            if let Some(gate) = gate {
                                let _ = gate.await;
                            }
                        }
                        STARTED => until(|| native.started.get()).await,
                        CLOSED => until(|| native.closes.get() > 0).await,
                        _ => native.pings.set(native.pings.get() + 1),
                    }
                    Ok(value + 1)
                }
            }),
    )
}

/// Which Orb serves `frames` to the consumer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Native,
    Source,
    Compiled,
}

struct Fixture {
    pool: LocalPool,
    host: Host,
    engine: Engine,
    reader: Rc<Reference>,
    producer: Rc<Reference>,
    native: Rc<Native>,
    logs: Arc<Mutex<Vec<String>>>,
    interpreted: bool,
    kind: Kind,
}

fn limits(timeout_ms: u64) -> Limits {
    Limits {
        timeout_ms,
        fuel: 20_000_000_000,
        ..Limits::default()
    }
}

fn engine(interpreted: bool, limits: Limits) -> (Engine, Arc<Mutex<Vec<String>>>) {
    let logs = Arc::<Mutex<Vec<String>>>::default();
    let sink = logs.clone();
    let engine = if interpreted {
        Engine::interpreted(limits)
    } else {
        Engine::new(limits)
    }
    .unwrap()
    .on_log(move |record| sink.lock().unwrap().push(record.message.clone()));
    (engine, logs)
}

/// The prepared consumer: revision 1 (`stream-reader`) or its replacement (`stream-reader-v2`).
fn consumer(engine: &Engine, revision: u32) -> Orb {
    engine
        .load(
            &fixture(if revision == 1 {
                "stream-reader"
            } else {
                "stream-reader-v2"
            }),
            Install::default()
                .requires(FRAMES, "frames", 1)
                .requires(ALIASED, "aliased", 1),
        )
        .unwrap()
}

fn relay(engine: &Engine) -> Orb {
    engine
        .load(
            &fixture("stream-relay"),
            Install::default().requires(FRAMES, "frames", 1),
        )
        .unwrap()
}

fn producer(engine: &Engine, kind: Kind, native: &Rc<Native>, compiled: Option<&[u8]>) -> Orb {
    match kind {
        Kind::Native => self::native(native.clone(), "frames", "producer-orb"),
        Kind::Source => engine
            .load(&fixture("stream-producer"), Install::default())
            .unwrap(),
        Kind::Compiled => engine
            .load(
                &Contract::from_dir(
                    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/wit/streams"),
                    Some("producer"),
                )
                .unwrap()
                .component_artifact(
                    compiled.expect("compiled producer"),
                    "frames",
                    1,
                    &Default::default(),
                )
                .unwrap(),
                Install::default(),
            )
            .unwrap(),
    }
}

fn install(interpreted: bool, limits: Limits, kind: Kind, compiled: Option<&[u8]>) -> Fixture {
    let (engine, logs) = engine(interpreted, limits);
    let pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let native = Rc::<Native>::default();
    host.install(vec![
        producer(&engine, kind, &native, compiled),
        self::native(native.clone(), "aliased", "aliased-orb"),
        consumer(&engine, 1),
    ])
    .unwrap();
    let reader = Rc::new(host.get(READER, vec!["main".into()], "alice").unwrap());
    let producer = Rc::new(host.get(PRODUCER, vec!["main".into()], "alice").unwrap());
    let mut fixture = Fixture {
        pool,
        host,
        engine,
        reader,
        producer,
        native,
        logs,
        interpreted,
        kind,
    };
    let (reader, producer) = (fixture.reader.clone(), fixture.producer.clone());
    fixture.pool.run_until(reader.ready()).unwrap();
    fixture.pool.run_until(producer.ready()).unwrap();
    fixture
}

/// Run `body` once on Cranelift and once on Pulley.
fn engines(body: impl Fn(bool)) {
    for interpreted in [false, true] {
        body(interpreted);
    }
}

impl Fixture {
    fn mode(&self) -> String {
        format!(
            "{} {:?}",
            if self.interpreted {
                "pulley"
            } else {
                "cranelift"
            },
            self.kind
        )
    }

    /// Call the consumer as `alice`, in a fresh root; also return that root.
    fn call_in_root<A, R>(
        &mut self,
        method: &'static str,
        arguments: A,
    ) -> (tembra::Result<R>, core::Id)
    where
        A: serde::Serialize + 'static,
        R: serde::de::DeserializeOwned + 'static,
    {
        let reference = self.reader.clone();
        let root = Rc::new(Cell::new(None));
        let seen = root.clone();
        let result = self.pool.run_until(self.host.root("alice", move |context| {
            seen.set(Some(context.root));
            async move { reference.call_portable(&context, method, arguments).await }.boxed_local()
        }));
        if let Err(error) = &result {
            eprintln!(
                "{}: {method} failed: {error:?}; logs {:?}",
                self.mode(),
                self.logs.lock().unwrap()
            );
        }
        (result, root.get().expect("the root ran"))
    }

    fn call<A, R>(&mut self, method: &'static str, arguments: A) -> tembra::Result<R>
    where
        A: serde::Serialize + 'static,
        R: serde::de::DeserializeOwned + 'static,
    {
        self.call_in_root(method, arguments).0
    }

    fn read(
        &mut self,
        count: u32,
        size: u32,
        mode: u32,
        take: u64,
        style: u32,
    ) -> tembra::Result<(u64, u32, u32)> {
        self.call("read", (count, size, mode, take, style))
    }

    fn calls(&self) -> usize {
        self.host.kernel().diagnostics().calls
    }

    fn logged(&self, message: &str) -> bool {
        self.logs.lock().unwrap().iter().any(|line| line == message)
    }

    /// Let the Stores' threads work until `done` holds, polling this host meanwhile.
    fn wait(&mut self, mut done: impl FnMut(&Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !done(self) {
            assert!(
                Instant::now() < deadline,
                "{}: the condition never held; logs {:?}",
                self.mode(),
                self.logs.lock().unwrap()
            );
            self.pool.run_until_stalled();
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Every call settled, no exported resource is held, and the consumer still serves.
    fn settled(&mut self) {
        self.wait(|fixture| fixture.calls() == 0);
        assert_eq!(self.engine.resources(), 0, "{}", self.mode());
        let mode = self.mode();
        assert_eq!(
            self.call::<_, u32>("ping", (41_u32,)).unwrap(),
            42,
            "{mode}"
        );
    }

    /// The producer's own scalar call.
    fn producer_ping(&mut self) -> tembra::Result<u32> {
        let reference = self.producer.clone();
        self.pool.run_until(self.host.root("alice", move |context| {
            async move { reference.call_portable(&context, "ping", (1_u32,)).await }.boxed_local()
        }))
    }

    /// Close the host; every kernel record and Store must be gone.
    fn close(mut self) {
        self.reader.release().unwrap();
        self.producer.release().unwrap();
        self.pool.run_until(self.host.close()).unwrap();
        self.closed();
    }

    /// Close a host where a revision was quarantined: closing reports its trap, and every kernel
    /// record and Store is still physically gone.
    fn retire(mut self) {
        let _ = self.reader.release();
        let _ = self.producer.release();
        // Closing may report the trap; the Stores are retired either way.
        let _ = self.pool.run_until(self.host.close());
        self.closed();
    }

    fn closed(&self) {
        let diagnostics = self.host.kernel().diagnostics();
        assert_eq!(
            (
                diagnostics.calls,
                diagnostics.references,
                diagnostics.roots,
                diagnostics.objects
            ),
            (0, 0, 0, 0),
            "{}",
            self.mode()
        );
        assert_eq!(self.engine.instances(), 0, "{}", self.mode());
        assert_eq!(self.engine.resources(), 0, "{}", self.mode());
    }

    /// The consumer revision was quarantined: its next ordinary call is refused.
    fn consumer_quarantined(&mut self) {
        let mode = self.mode();
        let refused = self.call::<_, u32>("ping", (41_u32,)).unwrap_err();
        assert_eq!(
            refused.reason, "wasm_executor_quarantined",
            "{mode}: {refused:?}"
        );
    }

    /// The host recorded a component trap.
    fn recorded_trap(&self) {
        let errors = self.host.errors();
        assert!(
            errors
                .iter()
                .any(|error| error.message.contains("component trap")),
            "{}: {errors:?}",
            self.mode()
        );
    }
}

/// More than a pipe, not aligned to 64 KiB chunks.
const COUNT: u32 = 5;
const SIZE: u32 = 40_000;

/// `readChunks` reads every byte exactly from each kind of producer. The `u64` limit crosses as
/// `u64::MAX` (not representable by a JavaScript number) and the `u64` result comes back exact.
fn reads_exactly(fixture: &mut Fixture) {
    let mode = fixture.mode();
    let (read, root) =
        fixture.call_in_root::<_, (u64, u32, u32)>("read", (COUNT, SIZE, CLEAN, u64::MAX, CHUNKS));
    let (bytes, mismatches, reads) = read.unwrap();
    assert_eq!(bytes, u64::from(COUNT * SIZE), "{mode}");
    assert_eq!(mismatches, 0, "{mode}");
    // Each read is at most one pipe.
    assert!(
        reads as usize >= (COUNT * SIZE) as usize / PIPE_BYTES,
        "{mode}: {reads}"
    );
    if fixture.kind == Kind::Native {
        // The producer ran in the consumer's root, as its principal.
        assert_eq!(
            fixture.native.calls.borrow().last().cloned(),
            Some(("alice".to_owned(), root)),
            "{mode}"
        );
    }
    if fixture.kind == Kind::Source {
        fixture.wait(|fixture| fixture.logged(&format!("producer:returned:{COUNT}")));
    }
    // A bounded `take` stops early after exact bytes, then closes the import.
    let (bytes, mismatches, _) = fixture.read(COUNT, SIZE, CLEAN, 70_000, CHUNKS).unwrap();
    assert!(
        (70_000..u64::from(COUNT * SIZE)).contains(&bytes),
        "{mode}: {bytes}"
    );
    assert_eq!(mismatches, 0, "{mode}");
    fixture.settled();
    assert_eq!(fixture.producer_ping().unwrap(), 2, "{mode}");
    assert!(
        fixture.host.errors().is_empty(),
        "{mode}: {:?}",
        fixture.host.errors()
    );
}

#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn a_source_consumer_reads_native_and_source_producers_exactly() {
    engines(|interpreted| {
        for kind in [Kind::Native, Kind::Source] {
            let mut fixture = install(interpreted, limits(30_000), kind, None);
            reads_exactly(&mut fixture);
            if kind == Kind::Native {
                // The jco per-byte iterator, across more than one pipe.
                let mode = fixture.mode();
                let (bytes, mismatches, _) =
                    fixture.read(2, 33_000, CLEAN, u64::MAX, BYTES).unwrap();
                assert_eq!((bytes, mismatches), (66_000, 0), "{mode}");
                // An aliased `stream<u8>` import.
                let (bytes, mismatches, _) = fixture
                    .read(COUNT, SIZE, CLEAN, u64::MAX, ALIAS + CHUNKS)
                    .unwrap();
                assert_eq!((bytes, mismatches), (u64::from(COUNT * SIZE), 0), "{mode}");
                fixture.settled();
            }
            fixture.close();
        }
    });
}

#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES and TEMBRA_WASM_STREAM_PRODUCER"]
fn a_source_consumer_reads_a_compiled_producer_exactly_and_sees_its_trap() {
    let compiled = compiled_producer();
    engines(|interpreted| {
        let mut fixture = install(interpreted, limits(30_000), Kind::Compiled, Some(&compiled));
        reads_exactly(&mut fixture);
        // The compiled producer traps after its whole body while holding its writer. The
        // consumer's read fails at its physical drain: its callback traps instead of returning a
        // truncated result, and both revisions are quarantined (the producer trapped itself).
        let mode = fixture.mode();
        // The body exceeds the 64 KiB pipe, so the trap cannot happen before the head was
        // delivered and the importer started draining (a tiny body may trap first, which fails
        // the import itself instead).
        let error = fixture
            .call::<_, (u64, String)>("failure", (128_u32, 4096_u32, FAIL_AFTER))
            .unwrap_err();
        assert!(error.message.contains("trap"), "{mode}: {error:?}");
        fixture.wait(|fixture| fixture.calls() == 0);
        fixture.recorded_trap();
        fixture.consumer_quarantined();
        let refused = fixture.producer_ping().unwrap_err();
        assert_eq!(
            refused.reason, "wasm_executor_quarantined",
            "{mode}: {refused:?}"
        );
        fixture.retire();

        // Through a source relay the native reader sees the exact prefix, then an error: never a
        // clean end. The relay's import failed, so the relay is quarantined too.
        let mut fixture = install(interpreted, limits(30_000), Kind::Compiled, Some(&compiled));
        let relayed = install_relay(&mut fixture);
        let (received, failure) =
            read_relayed(&mut fixture, &relayed, "frames", FAIL_AFTER, usize::MAX).unwrap();
        relayed_failure(
            &mut fixture,
            &relayed,
            &received,
            failure,
            (COUNT * SIZE) as usize,
        );
        let refused = fixture.producer_ping().unwrap_err();
        assert_eq!(
            refused.reason, "wasm_executor_quarantined",
            "{mode}: {refused:?}"
        );
        relayed.release().unwrap();
        fixture.retire();
    });
}

/// The head reaches the consumer before its producer finished, and the producer runs ahead only
/// by the bounded buffers.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn the_head_arrives_early_and_the_producer_is_backpressured() {
    engines(|interpreted| {
        let mut fixture = install(interpreted, limits(30_000), Kind::Native, None);
        let mode = fixture.mode();
        let (count, size) = (256_u32, 4096_u32);
        let (bytes, mismatches, _) = fixture.read(count, size, CLEAN, u64::MAX, PROBED).unwrap();
        assert_eq!((bytes, mismatches), (u64::from(count * size), 0), "{mode}");
        let probed = fixture.native.probed.get().expect("probed");
        // The consumer's first read (at most one pipe), the refilled pipe, the chunk the pump
        // holds and the one in flight: far below the whole 1 MiB body.
        assert!(
            probed * size as usize <= 2 * PIPE_BYTES + 2 * size as usize,
            "{mode}: {probed}"
        );
        assert!(probed < count as usize, "{mode}");
        fixture.settled();
        fixture.close();
    });
}

/// What a native caller read from the relay: the bytes, then a failure if the read failed.
type Relayed = (Vec<u8>, Option<tembra::Error>);

/// Install the relay beside `fixture`'s Orbs and return its reference.
fn install_relay(fixture: &mut Fixture) -> Rc<Reference> {
    fixture.host.install(vec![relay(&fixture.engine)]).unwrap();
    let relayed = Rc::new(
        fixture
            .host
            .get(RELAY, vec!["main".into()], "alice")
            .unwrap(),
    );
    fixture.pool.run_until(relayed.ready()).unwrap();
    relayed
}

/// Read at most `take` bytes of the relayed `method(COUNT, SIZE, mode)`, then close it: `frames`
/// returns the imported stream itself, `chunked` copies its chunks into a new stream.
fn read_relayed(
    fixture: &mut Fixture,
    relayed: &Rc<Reference>,
    method: &'static str,
    mode: u32,
    take: usize,
) -> tembra::Result<Relayed> {
    let relayed = relayed.clone();
    fixture
        .pool
        .run_until(fixture.host.root("alice", move |context| {
            async move {
                let mut stream = relayed
                    .call_portable::<_, ByteStream>(&context, method, (COUNT, SIZE, mode))
                    .await?;
                let mut received = Vec::new();
                while received.len() < take {
                    match futures::StreamExt::next(&mut stream).await {
                        Some(Ok(chunk)) => received.extend(chunk),
                        Some(Err(error)) => return Ok((received, Some(error))),
                        None => return Ok((received, None)),
                    }
                }
                stream.close().await?;
                Ok((received, None))
            }
            .boxed_local()
        }))
}

/// A relayed origin failure after its head: the native reader got an exact prefix of at most
/// `most` bytes and then an error (never a clean end), the relay trapped at its physical drain and
/// is quarantined, and nothing outlives the calls.
fn relayed_failure(
    fixture: &mut Fixture,
    relayed: &Rc<Reference>,
    received: &[u8],
    failure: Option<tembra::Error>,
    most: usize,
) {
    let mode = fixture.mode();
    assert!(received.len() <= most, "{mode}: {}", received.len());
    assert!(exact(received, SIZE as usize), "{mode}");
    let failure = failure.unwrap_or_else(|| {
        panic!(
            "{mode}: a relayed truncated stream ended cleanly after {} bytes",
            received.len()
        )
    });
    eprintln!("{mode}: relayed failure after the head: {failure:?}");
    fixture.wait(|fixture| fixture.calls() == 0);
    let refused = read_relayed(fixture, relayed, "frames", CLEAN, usize::MAX).unwrap_err();
    assert_eq!(
        refused.reason, "wasm_executor_quarantined",
        "{mode}: the relay kept serving after its import failed: {refused:?}"
    );
    fixture.wait(|fixture| fixture.calls() == 0);
}

/// A native caller reads a source relay that returns another source Orb's (or a native Orb's)
/// stream as its own whole result, directly or chunk by chunk: exact bytes, and a close that
/// reaches the origin.
///
/// An origin failure past its head is never a clean relayed end: the native reader receives an
/// exact prefix and then an error, because the relay's next read of its import fails and the
/// relay traps at its physical drain (it is quarantined). A source origin that trapped is
/// quarantined and its trap recorded; a native origin stays healthy. An origin failing before its
/// head fails the relay's import, which has no WIT error lane: the relay traps and is
/// quarantined, and the caller's call fails. Each failure runs on its own installation.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn a_source_relay_forwards_a_stream_from_source_or_native_to_native() {
    engines(|interpreted| {
        for kind in [Kind::Source, Kind::Native] {
            // Exact bytes, the imported stream returned directly or copied chunk by chunk.
            let mut fixture = install(interpreted, limits(30_000), kind, None);
            let mode = fixture.mode();
            let relayed = install_relay(&mut fixture);
            for method in ["frames", "chunked"] {
                let (received, failure) =
                    read_relayed(&mut fixture, &relayed, method, CLEAN, usize::MAX).unwrap();
                assert!(failure.is_none(), "{mode} {method}: {failure:?}");
                assert_eq!(received.len(), (COUNT * SIZE) as usize, "{mode} {method}");
                assert!(exact(&received, SIZE as usize), "{mode} {method}");
            }
            fixture.wait(|fixture| fixture.calls() == 0);
            relayed.release().unwrap();
            assert!(
                fixture.host.errors().is_empty(),
                "{mode}: {:?}",
                fixture.host.errors()
            );
            fixture.settled();
            assert_eq!(fixture.producer_ping().unwrap(), 2, "{mode}");
            fixture.close();

            // A failure after the origin's head.
            let mut fixture = install(interpreted, limits(30_000), kind, None);
            let relayed = install_relay(&mut fixture);
            let (received, failure) =
                read_relayed(&mut fixture, &relayed, "frames", FAIL_AFTER, usize::MAX).unwrap();
            relayed_failure(
                &mut fixture,
                &relayed,
                &received,
                failure,
                (COUNT / 2 * SIZE) as usize,
            );
            if kind == Kind::Native {
                // The native origin's function did not trap: it keeps serving.
                assert_eq!(fixture.producer_ping().unwrap(), 2, "{mode}");
            } else {
                // The source origin trapped and was quarantined; the host recorded the trap.
                fixture.recorded_trap();
                let refused = fixture.producer_ping().unwrap_err();
                assert_eq!(
                    refused.reason, "wasm_executor_quarantined",
                    "{mode}: {refused:?}"
                );
            }
            relayed.release().unwrap();
            fixture.retire();

            // A failure before the origin's head fails the relayed call.
            let mut fixture = install(interpreted, limits(30_000), kind, None);
            let relayed = install_relay(&mut fixture);
            let error = read_relayed(&mut fixture, &relayed, "frames", FAIL_BEFORE, usize::MAX)
                .unwrap_err();
            assert!(error.message.contains("trap"), "{mode}: {error:?}");
            fixture.wait(|fixture| fixture.calls() == 0);
            let refused =
                read_relayed(&mut fixture, &relayed, "frames", CLEAN, usize::MAX).unwrap_err();
            assert_eq!(
                refused.reason, "wasm_executor_quarantined",
                "{mode}: {refused:?}"
            );
            fixture.wait(|fixture| fixture.calls() == 0);
            relayed.release().unwrap();
            fixture.retire();
        }
        // Last, for every origin (blocked on the stream-close request in COORDINATION):
        // closing the relayed stream after one chunk of a quiet origin reaches the origin,
        // which stops.
        for kind in [Kind::Source, Kind::Native] {
            let mut fixture = install(interpreted, limits(30_000), kind, None);
            let mode = fixture.mode();
            let relayed = install_relay(&mut fixture);
            let (received, failure) =
                read_relayed(&mut fixture, &relayed, "frames", QUIET, 1).unwrap();
            assert!(
                failure.is_none() && received.len() == SIZE as usize,
                "{mode}: {failure:?}"
            );
            if kind == Kind::Native {
                // The quiet origin was closed and ended physically.
                fixture.wait(|fixture| {
                    fixture.native.closes.get() == 1 && fixture.native.ended.get() == 1
                });
            } else {
                fixture.wait(|fixture| fixture.logged("producer:returned"));
            }
            fixture.wait(|fixture| fixture.calls() == 0);
            relayed.release().unwrap();
            assert!(
                fixture.host.errors().is_empty(),
                "{mode}: {:?}",
                fixture.host.errors()
            );
            fixture.settled();
            assert_eq!(fixture.producer_ping().unwrap(), 2, "{mode}");
            fixture.close();
        }
    });
}

/// Returning without reading, stopping after a prefix, and an explicit close each ask the
/// producer to stop at once, never through the timeout; its physical end is still awaited.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn returning_early_or_closing_stops_the_producer() {
    engines(|interpreted| {
        let mut fixture = install(interpreted, limits(30_000), Kind::Native, None);
        let mode = fixture.mode();
        let started = Instant::now();
        // Returned while the producer is quiet.
        assert_eq!(
            fixture
                .call::<_, u32>("ignore", (4_u32, 1024_u32, QUIET))
                .unwrap(),
            7
        );
        fixture.wait(|fixture| fixture.native.closes.get() == 1 && fixture.native.ended.get() == 1);
        // Returned after a prefix while the producer is backpressured.
        let (bytes, _, _) = fixture.read(256, 4096, CLEAN, 8192, CHUNKS).unwrap();
        assert!(bytes >= 8192, "{mode}");
        fixture.wait(|fixture| fixture.native.closes.get() == 2 && fixture.native.ended.get() == 2);
        // An explicit close reaches the producer while the callback still runs.
        assert_eq!(
            fixture
                .call::<_, u32>("closing", (4_u32, 1024_u32))
                .unwrap(),
            CLOSED + 1
        );
        fixture.wait(|fixture| fixture.native.closes.get() == 3 && fixture.native.ended.get() == 3);
        assert!(started.elapsed() < Duration::from_secs(10), "{mode}");
        fixture.settled();
        fixture.close();

        // A source producer, backpressured or quiet, ends its callback when its reader is gone.
        let mut fixture = install(interpreted, limits(30_000), Kind::Source, None);
        let mode = fixture.mode();
        let started = Instant::now();
        let (bytes, _, _) = fixture.read(256, 4096, CLEAN, 4096, CHUNKS).unwrap();
        assert!(bytes >= 4096, "{mode}");
        fixture.wait(|fixture| fixture.calls() == 0);
        let returned = fixture
            .logs
            .lock()
            .unwrap()
            .iter()
            .find_map(|line| line.strip_prefix("producer:returned:").map(str::to_owned))
            .expect("the producer returned");
        assert!(returned.parse::<u32>().unwrap() < 256, "{mode}: {returned}");
        assert_eq!(
            fixture
                .call::<_, u32>("ignore", (2_u32, 1024_u32, QUIET))
                .unwrap(),
            7
        );
        fixture.wait(|fixture| fixture.calls() == 0);
        assert!(started.elapsed() < Duration::from_secs(10), "{mode}");
        assert_eq!(fixture.producer_ping().unwrap(), 2, "{mode}");
        fixture.settled();
        fixture.close();
    });
}

/// A producer failure is never a clean end of exact data. Breaking (declared component
/// semantics):
/// - before the head, the import fails; it has no WIT error lane, so the consumer traps and its
///   revision is quarantined (a source producer's uncaught exception quarantines it too);
/// - after the head, the consumer's next read fails at its physical drain: its callback traps
///   instead of returning a truncated result, and the consumer is quarantined (never a successful
///   `ping` afterwards). A source producer's raw exception traps it too, so the host records the
///   trap and quarantines the producer; a native producer's function stays healthy. The exact
///   prefix before the failure is asserted through the relay
///   (`a_source_relay_forwards_a_stream_from_source_or_native_to_native`).
///
/// Each failure runs on its own installation.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn producer_failures_are_errors_before_and_after_the_head() {
    engines(|interpreted| {
        for kind in [Kind::Native, Kind::Source] {
            let mut fixture = install(interpreted, limits(30_000), kind, None);
            let mode = fixture.mode();
            let error = fixture
                .call::<_, (u64, String)>("failure", (4_u32, 1024_u32, FAIL_BEFORE))
                .unwrap_err();
            assert!(error.message.contains("trap"), "{mode}: {error:?}");
            fixture.wait(|fixture| fixture.calls() == 0);
            fixture.consumer_quarantined();
            if kind == Kind::Native {
                assert_eq!(fixture.producer_ping().unwrap(), 2, "{mode}");
            } else {
                let refused = fixture.producer_ping().unwrap_err();
                assert_eq!(
                    refused.reason, "wasm_executor_quarantined",
                    "{mode}: {refused:?}"
                );
            }
            fixture.retire();

            let after_head = if kind == Kind::Source {
                vec![FAIL_AFTER, THROW_UNDEFINED]
            } else {
                vec![FAIL_AFTER]
            };
            for producer in after_head {
                let mut fixture = install(interpreted, limits(30_000), kind, None);
                let error = fixture
                    .call::<_, (u64, String)>("failure", (4_u32, 1024_u32, producer))
                    .unwrap_err();
                assert!(
                    error.message.contains("trap"),
                    "{mode} {producer}: {error:?}"
                );
                fixture.wait(|fixture| fixture.calls() == 0);
                fixture.consumer_quarantined();
                if kind == Kind::Native {
                    assert_eq!(fixture.producer_ping().unwrap(), 2, "{mode}");
                    fixture.retire();
                } else {
                    fixture.recorded_trap();
                    let refused = fixture.producer_ping().unwrap_err();
                    assert_eq!(
                        refused.reason, "wasm_executor_quarantined",
                        "{mode} {producer}: {refused:?}"
                    );
                    fixture.retire();
                }
            }
        }
    });
}

/// Root cancellation ends the import without quarantine; the callback timeout is terminal for
/// the consumer. Either way the producer is closed and physically ends.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn cancellation_and_timeout_close_the_producer() {
    engines(|interpreted| {
        let mut fixture = install(interpreted, limits(30_000), Kind::Native, None);
        let mode = fixture.mode();
        let reference = fixture.reader.clone();
        let outcome = fixture
            .pool
            .run_until(fixture.host.root("alice", move |context| {
                async move {
                    let read = reference.call_portable::<_, (u64, u32, u32)>(
                        &context,
                        "read",
                        (4_u32, 1024_u32, QUIET, u64::MAX, CHUNKS),
                    );
                    let cancel = async {
                        futures_timer::Delay::new(Duration::from_millis(200)).await;
                        context.cancel()
                    };
                    let (read, cancelled) = futures::join!(read, cancel);
                    cancelled?;
                    Ok(read)
                }
                .boxed_local()
            }));
        eprintln!("{mode}: cancelled read: {outcome:?}");
        // The cancellation fails the call, or the consumer returns what it read before its
        // import ended; it is never a quarantine.
        match outcome {
            Ok(Ok((bytes, mismatches, _))) => {
                assert!(
                    bytes <= 4096 && mismatches == 0,
                    "{mode}: {bytes} {mismatches}"
                )
            }
            Ok(Err(error)) | Err(error) => assert_ne!(
                error.reason, "wasm_executor_quarantined",
                "{mode}: {error:?}"
            ),
        }
        fixture.wait(|fixture| fixture.native.closes.get() == 1 && fixture.native.ended.get() == 1);
        fixture.settled();
        fixture.close();

        let mut fixture = install(interpreted, limits(1_500), Kind::Native, None);
        let mode = fixture.mode();
        let started = Instant::now();
        let error = fixture.read(4, 1024, QUIET, u64::MAX, CHUNKS).unwrap_err();
        assert!(started.elapsed() >= Duration::from_millis(1_400), "{mode}");
        assert!(error.message.contains("timed out"), "{mode}: {error:?}");
        fixture.wait(|fixture| {
            fixture.native.closes.get() == 1
                && fixture.native.ended.get() == 1
                && fixture.calls() == 0
        });
        fixture.consumer_quarantined();
        assert_eq!(fixture.producer_ping().unwrap(), 2, "{mode}");
        fixture.retire();
    });
}

/// A reload while the consumer reads lets the stream finish on the old revision; new calls reach
/// the replacement.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn a_reload_lets_an_open_import_finish_on_its_revision() {
    engines(|interpreted| {
        let mut fixture = install(interpreted, limits(30_000), Kind::Native, None);
        let mode = fixture.mode();
        // Prepare the replacement before the stream opens: its compilation is not part of the
        // open call's budget.
        let replacement = consumer(&fixture.engine, 2);
        let (open, gate) = oneshot::channel();
        *fixture.native.probe_gate.borrow_mut() = Some(gate);
        let reading = {
            let (reference, host) = (fixture.reader.clone(), fixture.host.clone());
            fixture
                .pool
                .spawner()
                .spawn_local_with_handle(async move {
                    host.root("alice", move |context| {
                        async move {
                            reference
                                .call_portable::<_, (u64, u32, u32)>(
                                    &context,
                                    "read",
                                    (64_u32, 4096_u32, CLEAN, u64::MAX, PROBED),
                                )
                                .await
                        }
                        .boxed_local()
                    })
                    .await
                })
                .unwrap()
        };
        fixture.wait(|fixture| fixture.native.probed.get().is_some());
        let reloading = fixture
            .pool
            .spawner()
            .spawn_local_with_handle({
                let host = fixture.host.clone();
                async move { host.reload(vec![replacement], 20_000).await }
            })
            .unwrap();
        fixture.pool.run_until_stalled();
        open.send(()).unwrap();
        let (bytes, mismatches, _) = fixture.pool.run_until(reading).unwrap();
        assert_eq!((bytes, mismatches), (64 * 4096, 0), "{mode}");
        fixture.pool.run_until(reloading).unwrap();
        fixture.wait(|fixture| fixture.calls() == 0);
        assert_eq!(
            fixture.call::<_, u32>("ping", (41_u32,)).unwrap(),
            43,
            "{mode}"
        );
        assert_eq!(fixture.engine.resources(), 0, "{mode}");
        fixture.close();
    });
}

/// Breaking: the private stream request packets are gone, so they cannot be forged. Each typed
/// import is the dependency's own WIT import, called through the reference the SDK selected in
/// the same turn. What a guest can still attempt is refused at the component boundary before
/// anything is called, catchably: arguments outside the import's signature (the selection is
/// cleared, so a valid call of the same callback goes on), a released reference, a reference
/// whose root claim ended with an earlier root, the context of an ended callback, and a binding
/// the contract does not import.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn typed_import_refusals_happen_before_any_call() {
    engines(|interpreted| {
        let mut fixture = install(interpreted, limits(30_000), Kind::Native, None);
        let mode = fixture.mode();
        // A reference acquired with `Context.get` serves its own root.
        assert_eq!(
            fixture.call::<_, u32>("retain", [0_u32; 0]).unwrap(),
            2,
            "{mode}"
        );
        assert_eq!(fixture.native.pings.get(), 1, "{mode}");
        for (which, expected) in [
            (0, "RangeError"),
            (2, "root"),
            (3, "Context callback has ended"),
            (4, "not a selected WIT Orb import"),
            // Last: a released reference that `select` failed to refuse traps the consumer.
            (1, "released"),
        ] {
            let refusal = fixture.call::<_, String>("forge", (which,)).unwrap();
            eprintln!("{mode} {which}: {refusal}");
            assert!(refusal.contains(expected), "{mode} {which}: {refusal}");
            assert!(
                !refusal.starts_with("accepted"),
                "{mode} {which}: {refusal}"
            );
            assert!(fixture.native.calls.borrow().is_empty(), "{mode} {which}");
        }
        // Besides `retain`'s own call, only the valid call after the refused argument reached the
        // producer.
        assert_eq!(fixture.native.pings.get(), 2, "{mode}");
        fixture.settled();
        assert!(fixture.native.calls.borrow().is_empty(), "{mode}");
        assert!(
            fixture.host.errors().is_empty(),
            "{mode}: {:?}",
            fixture.host.errors()
        );
        fixture.close();
    });
}

/// Sixteen imports may be outstanding at once; the seventeenth is refused and every delivered
/// end is closed and physically ended. Breaking: the refused import has no WIT error lane, so
/// the consumer traps and its revision is quarantined, as for a compiled importer.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn outstanding_stream_imports_are_bounded() {
    engines(|interpreted| {
        let mut fixture = install(interpreted, limits(30_000), Kind::Native, None);
        let mode = fixture.mode();
        let total: u64 = fixture.call("many", (16_u32, 20_u32, 4096_u32)).unwrap();
        assert_eq!(total, 16 * 20 * 4096, "{mode}");
        fixture.settled();
        let closes = fixture.native.closes.get();
        let error = fixture
            .call::<_, u64>("many", (17_u32, 20_u32, 4096_u32))
            .unwrap_err();
        assert!(
            error.message.contains("stream imports outstanding"),
            "{mode}: {error:?}"
        );
        // Every delivered end of the refused callback was closed and physically ended.
        fixture.wait(|fixture| {
            fixture.native.closes.get() == closes + 16
                && fixture.native.ended.get() == 32
                && fixture.calls() == 0
        });
        fixture.consumer_quarantined();
        assert_eq!(fixture.producer_ping().unwrap(), 2, "{mode}");
        fixture.retire();
    });
}

/// A callback that returns while its stream import is still pending is joined: its call settles
/// only after that import physically ended, and the late head is closed exactly once.
/// Breaking (DESIGN §3, detached work): returning no longer cancels the pending import. It stays
/// part of the call's task group, keeping the revision busy until it settles (or the call's
/// timeout quarantines the revision).
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn an_abandoned_pending_import_is_joined() {
    engines(|interpreted| {
        let mut fixture = install(interpreted, limits(30_000), Kind::Native, None);
        let mode = fixture.mode();
        let (release, physical) = oneshot::channel();
        *fixture.native.physical.borrow_mut() = Some(physical);
        let answer = Rc::new(RefCell::new(None));
        {
            let (reference, host, answer, native) = (
                fixture.reader.clone(),
                fixture.host.clone(),
                answer.clone(),
                fixture.native.clone(),
            );
            fixture
                .pool
                .spawner()
                .spawn_local(async move {
                    let result = host
                        .root("alice", move |context| {
                            async move {
                                reference
                                    .call_portable::<_, u32>(&context, "abandon", (4_u32, 1024_u32))
                                    .await
                            }
                            .boxed_local()
                        })
                        .await;
                    native.note("consumer:answer");
                    *answer.borrow_mut() = Some(result);
                })
                .unwrap();
        }
        fixture.wait(|fixture| fixture.native.started.get());
        // Physically held: the consumer's call has not settled, and nothing cancelled the import.
        for _ in 0..250 {
            fixture.pool.run_until_stalled();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(
            answer.borrow().is_none(),
            "{mode}: settled before the import's physical end"
        );
        assert_eq!(fixture.native.closes.get(), 0, "{mode}");
        assert_eq!(fixture.calls(), 2, "{mode}");
        assert!(
            !fixture
                .native
                .log
                .borrow()
                .iter()
                .any(|line| line == "gated:signalled"),
            "{mode}"
        );
        release.send(()).unwrap();
        fixture.wait(|_| answer.borrow().is_some());
        assert_eq!(
            answer.borrow_mut().take().unwrap().unwrap(),
            STARTED + 1,
            "{mode}"
        );
        assert!(
            fixture.native.position("gated:end") < fixture.native.position("consumer:answer"),
            "{mode}"
        );
        fixture.wait(|fixture| fixture.native.closes.get() == 1 && fixture.calls() == 0);
        fixture.settled();
        assert_eq!(fixture.native.closes.get(), 1, "{mode}");
        assert!(
            fixture.host.errors().is_empty(),
            "{mode}: {:?}",
            fixture.host.errors()
        );
        fixture.close();
    });
}

/// A producer calling back into its busy consumer is refused at once, not at a timeout.
/// Breaking: the producer's failure fails the consumer's import, which has no WIT error lane, so
/// the consumer traps and its revision is quarantined, as for a compiled importer.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn a_producer_calling_back_into_its_consumer_is_refused() {
    engines(|interpreted| {
        let mut fixture = install(interpreted, limits(5_000), Kind::Native, None);
        let mode = fixture.mode();
        *fixture.native.reenter.borrow_mut() = Some(fixture.reader.clone());
        let started = Instant::now();
        let error = fixture
            .call::<_, (u64, String)>("failure", (1_u32, 1_u32, REENTER))
            .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(2), "{mode}");
        assert!(error.message.contains("reentr"), "{mode}: {error:?}");
        fixture.native.reenter.borrow_mut().take();
        fixture.wait(|fixture| fixture.calls() == 0);
        fixture.consumer_quarantined();
        assert_eq!(fixture.producer_ping().unwrap(), 2, "{mode}");
        fixture.retire();
    });
}

/// Stream imports stay outside the public data profile (platform ports), and every other stream
/// shape stays refused for a source importer on the native facade too, before anything runs.
/// (`stream<u32>` is refused earlier still: the SDK build accepts no other numeric stream element,
/// so no artifact of it can exist.)
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn refused_import_profiles() {
    for interpreted in [false, true] {
        let (engine, _) = engine(interpreted, limits(30_000));
        let bindings = |names: &[&str]| -> BTreeMap<String, tembra_contracts::Binding> {
            names
                .iter()
                .map(|name| {
                    (
                        (*name).to_owned(),
                        tembra_contracts::Binding::Orb {
                            name: "frames".into(),
                            api_version: 1,
                        },
                    )
                })
                .collect()
        };
        let artifact = tembra_contracts::Artifact::decode(&fixture("stream-reader")).unwrap();
        let error = engine
            .open_port(
                &artifact,
                &bindings(&[FRAMES, ALIASED]),
                &Default::default(),
                &Default::default(),
            )
            .err()
            .expect("a platform port refuses stream imports");
        assert!(
            error
                .message
                .contains("served only through the native Rust facade"),
            "{error}"
        );
        for shape in [
            "stream-shape-sync",
            "stream-shape-argument",
            "stream-shape-list",
            "stream-shape-option",
            "stream-shape-future",
            "stream-shape-resource",
        ] {
            let error = engine
                .load(
                    &fixture(shape),
                    Install::default().requires("tembra-test:shapes/frames@1.0.0", "frames", 1),
                )
                .err()
                .unwrap_or_else(|| panic!("{shape} was admitted"));
            eprintln!("{shape}: {error}");
            assert!(!error.message.is_empty(), "{shape}");
        }
        assert_eq!(engine.instances(), 0);
    }
}
