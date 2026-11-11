//! A source (TypeScript) export's `stream<u8>` result consumed by native Rust as a `ByteStream`.
//!
//! The producer is the SDK-authored fixture `tests/source/stream-frames` (and its reload
//! replacement `stream-frames-v2`), prepared by the production SDK build into
//! `TEMBRA_WASM_FIXTURES`. Every case runs on Cranelift and on Pulley, through the real host,
//! kernel references and roots. The caller receives the stream end as soon as the guest
//! published it; the producer keeps writing in the same admitted export task, which ends only
//! after its byte source and the host relay have ended.
//!
//! Declared breaking semantics of the component runtime (COORDINATION, corrections 5 and 6):
//! - a raw producer exception, before or after the head, is not a typed business error: it traps
//!   the producer's instance and quarantines its revision. A native reader never sees a clean
//!   end.
//! - a host operation the host revokes (the receiving root ended or was cancelled) is the
//!   producer's to handle; unhandled, it is such a raw failure.
use futures::{FutureExt, StreamExt, channel::oneshot, executor::LocalPool, task::LocalSpawnExt};
#[path = "support/source.rs"]
mod source;
use source::fixture;
use std::{
    cell::RefCell,
    collections::BTreeMap,
    rc::Rc,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tembra::{Host, Orb, Reference, Service, portable::ByteStream};
use tembra_executor_wasm::{Contract, Engine, Install, Limits};

const SERVICE: &str = "frames::tembra-test:source-streams/frames@1.0.0";
const LEDGER: &str = "tembra-test:source-streams/ledger@1.0.0";
/// The pipe's byte capacity of the component host.
const PIPE_BYTES: usize = 64 << 10;

/// Producer modes of `tests/source/stream-frames/orb.ts`.
const CHUNKS: u32 = 0;
const BYTES: u32 = 1;
const ARRAY: u32 = 2;
const FAIL_AFTER_HEAD: u32 = 3;
const FAIL_BEFORE_HEAD: u32 = 4;
const NOT_A_STREAM: u32 = 5;
const IDLE: u32 = 6;
const DEPENDENCY: u32 = 7;
const THROW_UNDEFINED: u32 = 8;
const CLEANUP: u32 = 9;
const GATED: u32 = 10;
const NUMBERS: u32 = 11;
const RAW_ARRAY: u32 = 12;
const IDLE_HANDLED: u32 = 13;

/// The producer's contract, plus a provider world for the native ledger's signature.
fn contract(world: &str) -> Contract {
    let wit = format!(
        "{}\nworld ledger-provider {{ export ledger; }}\n",
        include_str!("source/stream-frames/contract.wit")
    );
    Contract::from_source("contract.wit", &wit, Some(world)).unwrap()
}

/// The prepared producer: revision 1 (`stream-frames`) or its replacement (`stream-frames-v2`).
fn artifact(revision: u32) -> Vec<u8> {
    fixture(if revision == 1 {
        "stream-frames"
    } else {
        "stream-frames-v2"
    })
}

/// What the native ledger saw: `(event, principal, root)` of each call.
type Seen = Rc<RefCell<Vec<(String, String, String)>>>;

/// Gated ledger calls, and the order of what they and the test observed.
#[derive(Default)]
struct Gates {
    log: RefCell<Vec<String>>,
    /// Held by a `gated` call after it observed its own cancellation: its physical end.
    physical: RefCell<Option<oneshot::Receiver<()>>>,
    /// Held by an `unrelated` call of the consumer's own root.
    unrelated: RefCell<Option<oneshot::Receiver<()>>>,
}

impl Gates {
    fn note(&self, entry: impl Into<String>) {
        self.log.borrow_mut().push(entry.into());
    }

    fn logged(&self, entry: &str) -> bool {
        self.log.borrow().iter().any(|line| line == entry)
    }

    fn position(&self, entry: &str) -> usize {
        let log = self.log.borrow();
        log.iter()
            .position(|line| line == entry)
            .unwrap_or_else(|| panic!("{entry} not logged: {log:?}"))
    }
}

/// The native dependency the producer calls after its head. `gated` waits for its own
/// cancellation, then for a separate physical gate; `unrelated` waits for its gate and reports
/// whether its signal was cancelled.
fn ledger(seen: Seen, gates: Rc<Gates>) -> Orb {
    let service = contract("ledger-provider")
        .plan("ledger", 1, &Default::default())
        .unwrap()
        .orb
        .services
        .remove(0);
    Orb::new("ledger", 1).service(
        Service::new(service.name, |_| Ok(()))
            .signature(service.signature)
            .method("note", move |_, context, (event,): (String,)| {
                let seen = seen.clone();
                let gates = gates.clone();
                async move {
                    if event == "gated" {
                        gates.note("gated:start");
                        context.signal().cancelled().await;
                        gates.note("gated:signalled");
                        let physical = gates.physical.borrow_mut().take();
                        if let Some(physical) = physical {
                            let _ = physical.await;
                        }
                        gates.note("gated:end");
                        return Ok("gated".into());
                    }
                    if event == "unrelated" {
                        gates.note("unrelated:start");
                        let gate = gates.unrelated.borrow_mut().take();
                        if let Some(gate) = gate {
                            let _ = gate.await;
                        }
                        let cancelled = context.signal().is_cancelled();
                        gates.note(format!("unrelated:end:{cancelled}"));
                        return Ok("unrelated".into());
                    }
                    let principal = context.principal()?;
                    let root = format!("{:?}", context.root);
                    seen.borrow_mut()
                        .push((event, principal.clone(), root.clone()));
                    Ok(format!("{principal}@{root}"))
                }
            }),
    )
}

struct Fixture {
    pool: LocalPool,
    host: Host,
    engine: Engine,
    reference: Rc<Reference>,
    logs: Arc<Mutex<Vec<String>>>,
    seen: Seen,
    gates: Rc<Gates>,
    interpreted: bool,
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

fn install(interpreted: bool, limits: Limits) -> Fixture {
    let (engine, logs) = engine(interpreted, limits);
    let pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let seen = Seen::default();
    let gates = Rc::<Gates>::default();
    let producer = engine
        .load(
            &artifact(1),
            Install::default().requires(LEDGER, "ledger", 1),
        )
        .unwrap();
    host.install(vec![ledger(seen.clone(), gates.clone()), producer])
        .unwrap();
    let reference = Rc::new(host.get(SERVICE, vec!["main".into()], "alice").unwrap());
    let mut fixture = Fixture {
        pool,
        host,
        engine,
        reference,
        logs,
        seen,
        gates,
        interpreted,
    };
    let reference = fixture.reference.clone();
    fixture.pool.run_until(reference.ready()).unwrap();
    fixture
}

fn limits(timeout_ms: u64) -> Limits {
    Limits {
        timeout_ms,
        ..Limits::default()
    }
}

/// Run `body` once on Cranelift and once on Pulley.
fn engines(body: impl Fn(bool)) {
    for interpreted in [false, true] {
        body(interpreted);
    }
}

impl Fixture {
    /// Run a root as `alice`.
    fn root<T: 'static>(
        &mut self,
        operation: impl AsyncFnOnce(tembra::Context, Rc<Reference>, Host) -> tembra::Result<T> + 'static,
    ) -> tembra::Result<T> {
        let (reference, host) = (self.reference.clone(), self.host.clone());
        let result = self.pool.run_until(self.host.root("alice", move |context| {
            operation(context, reference, host).boxed_local()
        }));
        if let Err(error) = &result {
            eprintln!(
                "{}: root failed: {error:?}; logs {:?}; host errors {:?}",
                self.mode(),
                self.logs.lock().unwrap(),
                self.host.errors()
            );
        }
        result
    }

    fn calls(&self) -> usize {
        self.host.kernel().diagnostics().calls
    }

    fn logged(&self, message: &str) -> bool {
        self.logs.lock().unwrap().iter().any(|line| line == message)
    }

    /// Let the Store's thread work until `done` holds, polling this host meanwhile.
    fn wait(&mut self, mut done: impl FnMut(&Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !done(self) {
            assert!(
                Instant::now() < deadline,
                "{}: the condition never held; logs {:?}; gates {:?}; calls {}; host errors {:?}",
                self.mode(),
                self.logs.lock().unwrap(),
                self.gates.log.borrow(),
                self.calls(),
                self.host.errors()
            );
            self.pool.run_until_stalled();
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn mode(&self) -> &'static str {
        if self.interpreted {
            "pulley"
        } else {
            "cranelift"
        }
    }

    /// The revision serves an ordinary scalar call.
    fn ping(&mut self) -> tembra::Result<u32> {
        self.root(async move |context, reference, _| {
            reference
                .call_portable::<_, u32>(&context, "ping", (41_u32,))
                .await
        })
    }

    /// Close the host; every kernel record and Store must be gone.
    fn close(mut self) {
        self.reference.release().unwrap();
        self.pool.run_until(self.host.close()).unwrap();
        self.closed();
    }

    /// Close a host whose producer revision was quarantined: every kernel record and Store is
    /// still physically gone.
    fn retire(mut self) {
        let _ = self.reference.release();
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

    /// The producer revision traps or was quarantined: the next ordinary call is refused.
    fn quarantined(&mut self) {
        let mode = self.mode();
        let refused = self.ping().unwrap_err();
        assert_eq!(
            refused.reason, "wasm_executor_quarantined",
            "{mode}: {refused:?}"
        );
    }

    /// The host recorded the producer's trap (its failure after the head never ended cleanly).
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

/// Byte `j` of chunk `i` is `(i + j) mod 256`.
fn expected(offset: usize, size: usize) -> u8 {
    ((offset / size + offset % size) % 256) as u8
}

fn exact(received: &[u8], size: usize) -> bool {
    received
        .iter()
        .enumerate()
        .all(|(offset, byte)| *byte == expected(offset, size))
}

async fn open(
    context: &tembra::Context,
    reference: &Reference,
    count: u32,
    size: u32,
    mode: u32,
) -> tembra::Result<ByteStream> {
    reference
        .call_portable::<_, ByteStream>(context, "frames", (count, size, mode))
        .await
}

/// Read to the end; `Err` carries the bytes received before the failure.
async fn drain(stream: &mut ByteStream) -> Result<Vec<u8>, (Vec<u8>, tembra::Error)> {
    let mut received = Vec::new();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(chunk) => received.extend(chunk),
            Err(error) => return Err((received, error)),
        }
    }
    Ok(received)
}

/// The decisive case: 512 KiB, beyond the 64 KiB pipe, published before the producer finished.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn a_source_stream_head_arrives_while_its_producer_is_still_admitted() {
    engines(|interpreted| {
        let mut fixture = install(interpreted, limits(30_000));
        let (calls_at_head, after_pause, received) = fixture
            .root(async move |context, reference, host| {
                let mut stream = open(&context, &reference, 8, 64 << 10, CHUNKS).await?;
                // The head is here while the producer's callback is still admitted.
                let calls_at_head = host.kernel().diagnostics().calls;
                let mut received = stream.next().await.unwrap()?;
                // Not reading: the producer can run ahead only by the bounded buffers.
                std::thread::sleep(Duration::from_millis(200));
                let after_pause = stream.next().await.unwrap()?;
                received.extend_from_slice(&after_pause);
                received.extend(drain(&mut stream).await.map_err(|(_, error)| error)?);
                Ok((calls_at_head, after_pause.len(), received))
            })
            .unwrap();
        let mode = fixture.mode();
        assert_eq!(calls_at_head, 1, "{mode}");
        assert!(after_pause <= PIPE_BYTES, "{mode}: {after_pause}");
        assert_eq!(received.len(), 8 << 16, "{mode}");
        assert!(exact(&received, 64 << 10), "{mode}");
        fixture.wait(|fixture| fixture.calls() == 0);
        assert!(fixture.logged("producer:returned:8"), "{mode}");
        assert_eq!(fixture.ping().unwrap(), 42, "{mode}");
        assert!(fixture.host.errors().is_empty(), "{mode}");
        fixture.close();
    });
}

/// Per-byte `AsyncIterable<number>` and a bare `Uint8Array` (an iterable of numbers) are still
/// accepted: the runtime writes each non-chunk item as one byte. One-byte chunks and a whole
/// array as the single chunk of a synchronous iterable are the chunked forms of the same bytes.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn per_byte_iterables_and_byte_arrays_stream_exactly() {
    engines(|interpreted| {
        // Each item is written by its own JavaScript turn of the export task; thousands of them
        // need more fuel than the small default.
        let mut fixture = install(
            interpreted,
            Limits {
                fuel: 20_000_000_000,
                ..limits(30_000)
            },
        );
        let mode = fixture.mode();
        for (producer, count, size) in [
            (NUMBERS, 3, 1000),
            (RAW_ARRAY, 2, 5000),
            (BYTES, 3, 1000),
            (ARRAY, 5, 50_000),
        ] {
            let received = fixture
                .root(async move |context, reference, _| {
                    let mut stream = open(&context, &reference, count, size, producer).await?;
                    drain(&mut stream).await.map_err(|(_, error)| error)
                })
                .unwrap();
            assert_eq!(received.len(), (count * size) as usize, "{mode} {producer}");
            assert!(exact(&received, size as usize), "{mode} {producer}");
            fixture.wait(|fixture| fixture.calls() == 0);
        }
        assert_eq!(fixture.ping().unwrap(), 42, "{mode}");
        assert!(fixture.host.errors().is_empty(), "{mode}");
        fixture.close();
    });
}

/// Breaking: failing before the head, or returning something that is not a byte source, is an
/// uncaught exception of an export without a WIT `result`: the call fails with a trap and the
/// revision is quarantined (the removed interpreter kept serving). Nothing outlives the call.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn a_producer_without_a_head_fails_its_call_and_quarantines_its_revision() {
    engines(|interpreted| {
        for producer in [FAIL_BEFORE_HEAD, NOT_A_STREAM] {
            let mut fixture = install(interpreted, limits(30_000));
            let mode = format!("{} {producer}", fixture.mode());
            let error = fixture
                .root(async move |context, reference, _| {
                    open(&context, &reference, 1, 1, producer).await.map(drop)
                })
                .unwrap_err();
            assert!(
                error.message.contains("component trap"),
                "{mode}: {error:?}"
            );
            fixture.wait(|fixture| fixture.calls() == 0);
            fixture.quarantined();
            fixture.retire();
        }
    });
}

/// An author error after the head is never a clean end: an exact prefix, then an error. Breaking:
/// a raw producer exception is not a business error; it traps before the writer is dropped, the
/// host records the trap and the revision is quarantined.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn an_author_error_after_the_head_fails_the_read_and_quarantines_the_revision() {
    engines(|interpreted| {
        let mut fixture = install(interpreted, limits(30_000));
        let outcome = fixture
            .root(async move |context, reference, _| {
                let mut stream = open(&context, &reference, 16, 64 << 10, FAIL_AFTER_HEAD).await?;
                Ok(drain(&mut stream).await)
            })
            .unwrap();
        let mode = fixture.mode();
        let Err((prefix, error)) = outcome else {
            panic!("{mode}: a failed producer ended cleanly");
        };
        assert!(prefix.len() <= 8 << 16, "{mode}");
        assert!(exact(&prefix, 64 << 10), "{mode}");
        eprintln!("{mode}: failed read: {error:?}");
        fixture.wait(|fixture| fixture.calls() == 0);
        fixture.recorded_trap();
        fixture.quarantined();
        fixture.retire();
    });
}

/// After its head, the producer calls a dependency under the caller's root and principal.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn a_post_head_dependency_call_keeps_the_callers_root_and_principal() {
    engines(|interpreted| {
        let mut fixture = install(interpreted, limits(30_000));
        let (root, received) = fixture
            .root(async move |context, reference, _| {
                let root = format!("{:?}", context.root);
                let mut stream = open(&context, &reference, 4, 1 << 10, DEPENDENCY).await?;
                let received = drain(&mut stream).await.map_err(|(_, error)| error)?;
                Ok((root, received))
            })
            .unwrap();
        let mode = fixture.mode();
        assert_eq!(received.len(), 4 << 10, "{mode}");
        assert!(exact(&received, 1 << 10), "{mode}");
        assert_eq!(
            *fixture.seen.borrow(),
            vec![("after-head".to_owned(), "alice".to_owned(), root.clone())],
            "{mode}"
        );
        assert!(fixture.logged(&format!("ledger:alice@{root}")), "{mode}");
        fixture.wait(|fixture| fixture.calls() == 0);
        fixture.close();
    });
}

/// A consumer that drops its end while the producer is held by backpressure stops the producer:
/// its next write sees the closed reader, its source is returned, and the callback ends cleanly.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn dropping_a_backpressured_stream_stops_its_producer() {
    engines(|interpreted| {
        let mut fixture = install(interpreted, limits(30_000));
        let first = fixture
            .root(async move |context, reference, _| {
                let mut stream = open(&context, &reference, 256, 64 << 10, CHUNKS).await?;
                let first = stream.next().await.unwrap()?;
                // Let the producer fill both bounded buffers and park.
                std::thread::sleep(Duration::from_millis(200));
                drop(stream);
                Ok(first)
            })
            .unwrap();
        let mode = fixture.mode();
        assert!(exact(&first, 64 << 10), "{mode}");
        fixture.wait(|fixture| fixture.calls() == 0);
        // Returned early: at most the two bounded buffers and one write ran ahead.
        let returned = fixture
            .logs
            .lock()
            .unwrap()
            .iter()
            .find_map(|line| line.strip_prefix("producer:returned:")?.parse::<u32>().ok());
        assert!(
            returned.is_some_and(|index| index < 8),
            "{mode}: {returned:?}"
        );
        assert_eq!(fixture.ping().unwrap(), 42, "{mode}");
        assert!(fixture.host.errors().is_empty(), "{mode}");
        fixture.close();
    });
}

/// A consumer that drops its end while the producer is idle in its own wait ends the callback
/// at once; it does not wait for the revision's timeout. The consumer's root then ends, and its
/// cancellation revokes the producer's pending wait. A producer that handles the revocation ends
/// normally and keeps its revision. Breaking: an unhandled revocation is a raw producer failure
/// (no WIT error lane), so it traps and quarantines the revision, still at once and with
/// nothing outliving the call.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn dropping_a_stream_of_an_idle_producer_ends_its_callback() {
    engines(|interpreted| {
        for producer in [IDLE_HANDLED, IDLE] {
            let mut fixture = install(interpreted, limits(30_000));
            let mode = format!("{} {producer}", fixture.mode());
            let started = Instant::now();
            let first = Rc::new(RefCell::new(None));
            let received = first.clone();
            let outcome = fixture.root(async move |context, reference, _| {
                let mut stream = open(&context, &reference, 2, 1 << 10, producer).await?;
                *received.borrow_mut() = Some(stream.next().await.unwrap()?);
                drop(stream);
                Ok(())
            });
            assert!(exact(first.borrow().as_ref().unwrap(), 1 << 10), "{mode}");
            fixture.wait(|fixture| fixture.calls() == 0);
            assert!(started.elapsed() < Duration::from_secs(10), "{mode}");
            if producer == IDLE_HANDLED {
                outcome.unwrap();
                assert!(
                    fixture
                        .logs
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|line| line.starts_with("producer:revoked:")),
                    "{mode}"
                );
                assert_eq!(fixture.ping().unwrap(), 42, "{mode}");
                assert!(fixture.host.errors().is_empty(), "{mode}");
                fixture.close();
            } else {
                // The dropping root may also report the producer's trap.
                if let Err(error) = outcome {
                    assert!(error.message.contains("trap"), "{mode}: {error:?}");
                }
                assert!(fixture.logged("producer:returned"), "{mode}");
                fixture.recorded_trap();
                fixture.quarantined();
                fixture.retire();
            }
        }
    });
}

/// Cancelling the root of an idle producer fails the read. The revoked wait is the producer's to
/// handle: handled, the revision keeps serving; unhandled (breaking, as above), it traps and
/// quarantines the revision. Either way nothing outlives the call.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn cancelling_the_root_fails_the_read() {
    engines(|interpreted| {
        for producer in [IDLE_HANDLED, IDLE] {
            let mut fixture = install(interpreted, limits(30_000));
            let mode = format!("{} {producer}", fixture.mode());
            let outcome = fixture.root(async move |context, reference, _| {
                let mut stream = open(&context, &reference, 2, 1 << 10, producer).await?;
                let first = stream.next().await.unwrap()?;
                context.cancel()?;
                Ok((first, drain(&mut stream).await))
            });
            eprintln!("{mode}: cancelled root: {outcome:?}");
            match outcome {
                Ok((first, read)) => {
                    assert!(exact(&first, 1 << 10), "{mode}");
                    let Err((rest, error)) = read else {
                        panic!("{mode}: a cancelled producer ended cleanly");
                    };
                    assert!(rest.is_empty(), "{mode}");
                    assert_ne!(
                        error.reason, "wasm_executor_quarantined",
                        "{mode}: {error:?}"
                    );
                }
                Err(error) => assert_ne!(
                    error.reason, "wasm_executor_quarantined",
                    "{mode}: the cancelled root itself failed: {error:?}"
                ),
            }
            fixture.wait(|fixture| fixture.calls() == 0);
            if producer == IDLE_HANDLED {
                assert_eq!(fixture.ping().unwrap(), 42, "{mode}");
                assert!(
                    fixture.host.errors().is_empty(),
                    "{mode}: {:?}",
                    fixture.host.errors()
                );
                fixture.close();
            } else {
                fixture.recorded_trap();
                fixture.quarantined();
                fixture.retire();
            }
        }
    });
}

/// The whole export task, stream included, is bounded by the revision timeout. Expiry is terminal
/// (the selected policy): the read fails and the revision is quarantined.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn the_terminal_timeout_fails_the_read_and_quarantines_the_revision() {
    engines(|interpreted| {
        let mut fixture = install(interpreted, limits(1_500));
        let started = Instant::now();
        let outcome = fixture.root(async move |context, reference, _| {
            let mut stream = open(&context, &reference, 2, 1 << 10, IDLE).await?;
            Ok(drain(&mut stream).await)
        });
        let mode = fixture.mode();
        // The read fails after the head; the root may also report the expiry itself.
        let error = match outcome {
            Ok(Err((prefix, error))) => {
                assert_eq!(prefix.len(), 1 << 10, "{mode}");
                error
            }
            Ok(Ok(_)) => panic!("{mode}: a timed-out producer ended cleanly"),
            Err(error) => error,
        };
        assert!(started.elapsed() >= Duration::from_millis(1_400), "{mode}");
        assert!(error.message.contains("timed out"), "{mode}: {error:?}");
        fixture.wait(|fixture| fixture.calls() == 0);
        let refused = fixture.ping().unwrap_err();
        assert_eq!(
            refused.reason, "wasm_executor_quarantined",
            "{mode}: {refused:?}"
        );
        assert!(refused.message.contains("timed out"), "{mode}: {refused:?}");
        fixture.retire();
    });
}

/// A reload while a stream is being read lets the old revision finish it; new calls reach the
/// replacement, and closing the host releases everything.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn a_reload_lets_an_open_stream_finish_on_its_revision() {
    engines(|interpreted| {
        let mut fixture = install(interpreted, limits(30_000));
        // Prepare the replacement before the stream opens: its compilation is not part of the
        // open call's budget.
        let replacement = fixture
            .engine
            .load(
                &artifact(2),
                Install::default().requires(LEDGER, "ledger", 1),
            )
            .unwrap();
        let (sender, opened) = futures::channel::oneshot::channel();
        let (resume, resumed) = futures::channel::oneshot::channel::<()>();
        let reading = {
            let (reference, host) = (fixture.reference.clone(), fixture.host.clone());
            fixture
                .pool
                .spawner()
                .spawn_local_with_handle(async move {
                    host.root("alice", move |context| {
                        async move {
                            let mut stream =
                                open(&context, &reference, 8, 64 << 10, CHUNKS).await?;
                            let first = stream.next().await.unwrap()?;
                            let _ = sender.send(());
                            let _ = resumed.await;
                            let mut received = first;
                            received.extend(drain(&mut stream).await.map_err(|(_, error)| error)?);
                            Ok(received)
                        }
                        .boxed_local()
                    })
                    .await
                })
                .unwrap()
        };
        fixture.pool.run_until(opened).unwrap();
        let reloading = fixture
            .pool
            .spawner()
            .spawn_local_with_handle({
                let host = fixture.host.clone();
                async move { host.reload(vec![replacement], 20_000).await }
            })
            .unwrap();
        fixture.pool.run_until_stalled();
        resume.send(()).unwrap();
        let received = fixture.pool.run_until(reading).unwrap();
        let mode = fixture.mode();
        assert_eq!(received.len(), 8 << 16, "{mode}");
        assert!(exact(&received, 64 << 10), "{mode}");
        fixture.pool.run_until(reloading).unwrap();
        fixture.wait(|fixture| fixture.calls() == 0);
        assert_eq!(
            fixture.ping().unwrap(),
            43,
            "{mode}: the replacement serves"
        );
        fixture.close();
    });
}

/// A packet limit far below the 64 KiB write chunk never truncates a stream into a clean end.
/// Breaking: the component host carries stream bytes through its bounded pipe, never as host
/// packets, so the packet limit no longer applies to them: the stream arrives whole and exact,
/// while the limit still bounds packets.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn a_small_packet_limit_never_truncates_a_stream_into_a_clean_end() {
    engines(|interpreted| {
        let mut fixture = install(
            interpreted,
            Limits {
                packet_bytes: 16 << 10,
                ..limits(30_000)
            },
        );
        let received = fixture
            .root(async move |context, reference, _| {
                let mut stream = open(&context, &reference, 4, 64 << 10, CHUNKS).await?;
                drain(&mut stream).await.map_err(|(_, error)| error)
            })
            .unwrap();
        let mode = fixture.mode();
        assert_eq!(received.len(), 4 << 16, "{mode}");
        assert!(exact(&received, 64 << 10), "{mode}");
        fixture.wait(|fixture| fixture.calls() == 0);
        assert!(fixture.logged("producer:returned:4"), "{mode}");
        assert_eq!(fixture.ping().unwrap(), 42, "{mode}");
        assert!(fixture.host.errors().is_empty(), "{mode}");
        fixture.close();
    });
}

/// Data-only platform ports keep refusing a prepared source stream export: the public Node and
/// browser data profile never carries a stream end.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn platform_data_ports_refuse_source_stream_exports() {
    for interpreted in [false, true] {
        let (engine, _) = engine(interpreted, Limits::default());
        let artifact = tembra_contracts::Artifact::decode(&artifact(1)).unwrap();
        let bindings: BTreeMap<String, tembra_contracts::Binding> = artifact
            .contract
            .providers(
                &BTreeMap::from([(
                    LEDGER.to_owned(),
                    tembra_contracts::OrbProvider {
                        orb: "ledger".into(),
                        api_version: 1,
                    },
                )]),
                &Default::default(),
            )
            .unwrap();
        let error = engine
            .open_port(
                &artifact,
                &bindings,
                &Default::default(),
                &Default::default(),
            )
            .err()
            .expect("a data port refuses a stream export");
        assert!(error.message.contains("byte-stream"), "{error:?}");
        assert!(error.message.contains("data only"), "{error:?}");
        assert_eq!(engine.instances(), 0);
    }
}

/// Data-only platform ports keep refusing prepared source stream imports.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn platform_data_ports_refuse_source_stream_imports() {
    let artifact = tembra_contracts::Artifact::decode(&fixture("stream-reader")).unwrap();
    let bindings = artifact
        .contract
        .providers(
            &BTreeMap::from([
                (
                    "tembra-test:streams/frames@1.0.0".to_owned(),
                    tembra_contracts::OrbProvider {
                        orb: "frames".into(),
                        api_version: 1,
                    },
                ),
                (
                    "tembra-test:streams/aliased@1.0.0".to_owned(),
                    tembra_contracts::OrbProvider {
                        orb: "aliased".into(),
                        api_version: 1,
                    },
                ),
            ]),
            &Default::default(),
        )
        .unwrap();
    for interpreted in [false, true] {
        let (engine, _) = engine(interpreted, Limits::default());
        let error = engine
            .open_port(
                &artifact,
                &bindings,
                &Default::default(),
                &Default::default(),
            )
            .err()
            .expect("a data port refuses a stream import");
        assert!(error.message.contains("byte-stream"), "{error:?}");
        assert!(error.message.contains("data only"), "{error:?}");
        assert_eq!(engine.instances(), 0);
    }
}

/// A source may throw any value: `throw undefined` after its head is a failure, never a clean
/// end of the bytes it produced. Breaking: it traps and quarantines the revision, like any raw
/// producer exception.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn a_producer_throwing_undefined_fails_the_read() {
    engines(|interpreted| {
        let mut fixture = install(interpreted, limits(30_000));
        // The read's outcome, whatever the root reports.
        let read = Rc::new(RefCell::new(None));
        let seen = read.clone();
        let outcome = fixture.root(async move |context, reference, _| {
            let mut stream = open(&context, &reference, 4, 64 << 10, THROW_UNDEFINED).await?;
            *seen.borrow_mut() = Some(drain(&mut stream).await);
            Ok(())
        });
        let mode = fixture.mode();
        if let Err(error) = outcome {
            // The root may also report the producer's trap.
            assert!(error.message.contains("trap"), "{mode}: {error:?}");
        }
        let Some(Err((prefix, _))) = read.borrow_mut().take() else {
            panic!("{mode}: a producer throwing undefined ended cleanly");
        };
        assert!(prefix.len() <= 4 << 16, "{mode}");
        assert!(exact(&prefix, 64 << 10), "{mode}");
        fixture.wait(|fixture| fixture.calls() == 0);
        fixture.recorded_trap();
        fixture.quarantined();
        fixture.retire();
    });
}

/// Closing the stream cancels the producer's call, so the source's asynchronous cleanup
/// (`return()` and its `finally`) cannot perform host operations: its dependency call is never
/// admitted. A dependency call admitted before the close is cancelled and joined instead
/// (`closing_a_stream_cancels_and_joins_its_admitted_dependency`). Either way nothing outlives
/// the call, and the close is acknowledged only after physical settlement. Breaking (D5): the
/// refused cleanup call has no WIT error lane, so if the cleanup issues it, the producer traps;
/// the close then reports the trap and the revision is quarantined.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn closing_a_stream_never_leaves_its_cleanup_detached() {
    engines(|interpreted| {
        let mut fixture = install(interpreted, limits(30_000));
        let (first, closed) = fixture
            .root(async move |context, reference, _| {
                let mut stream = open(&context, &reference, 0, 1 << 10, CLEANUP).await?;
                let first = stream.next().await.unwrap()?;
                Ok((first, stream.close().await))
            })
            .unwrap();
        let mode = fixture.mode();
        assert!(exact(&first, 1 << 10), "{mode}");
        // The close is acknowledged only once the producer's call physically settled.
        assert_eq!(
            fixture.calls(),
            0,
            "{mode}: acknowledged before physical settlement"
        );
        // The cleanup's dependency call was never admitted.
        assert!(
            fixture.seen.borrow().is_empty(),
            "{mode}: {:?}",
            fixture.seen.borrow()
        );
        eprintln!("{mode}: close after a cleanup dependency call: {closed:?}");
        match closed {
            Ok(()) => {
                assert_eq!(fixture.ping().unwrap(), 42, "{mode}");
                assert!(fixture.host.errors().is_empty(), "{mode}");
                fixture.close();
            }
            Err(error) => {
                assert!(error.message.contains("trap"), "{mode}: {error:?}");
                fixture.quarantined();
                fixture.retire();
            }
        }
    });
}

/// B1: closing a stream while its producer awaits an admitted dependency call cancels exactly
/// that call and joins it. The call observes its own cancellation (logical) while its physical
/// gate is held; the close is not acknowledged until the call has physically ended. An
/// unrelated call of the consumer's own root keeps a live signal throughout.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn closing_a_stream_cancels_and_joins_its_admitted_dependency() {
    engines(|interpreted| {
        let mut fixture = install(interpreted, limits(30_000));
        let mode = fixture.mode();
        let gates = fixture.gates.clone();
        let (release_physical, physical) = oneshot::channel();
        let (release_unrelated, unrelated) = oneshot::channel();
        *gates.physical.borrow_mut() = Some(physical);
        *gates.unrelated.borrow_mut() = Some(unrelated);
        let ledger = Rc::new(
            fixture
                .host
                .get(&format!("ledger::{LEDGER}"), vec!["main".into()], "alice")
                .unwrap(),
        );
        let (opened, first_chunk) = oneshot::channel();
        let (close_now, close) = oneshot::channel::<()>();
        let root = {
            let (host, reference, gates) = (
                fixture.host.clone(),
                fixture.reference.clone(),
                gates.clone(),
            );
            fixture
                .pool
                .spawner()
                .spawn_local_with_handle(async move {
                    let counter = host.clone();
                    host.root("alice", move |context| {
                        async move {
                            let unrelated = ledger
                                .call_portable::<_, String>(&context, "note", ("unrelated",))
                                .boxed_local()
                                .shared();
                            let pending = unrelated.clone();
                            let mut stream = open(&context, &reference, 2, 1 << 10, GATED).await?;
                            let first = stream.next().await.unwrap()?;
                            let _ = opened.send(first);
                            // Keep the unrelated call admitted while the stream closes.
                            let (closed, ()) = futures::future::join(
                                async {
                                    let _ = close.await;
                                    gates.note("close:begin");
                                    let closed = stream.close().await;
                                    gates.note(format!(
                                        "close:ack:calls={}",
                                        counter.kernel().diagnostics().calls
                                    ));
                                    closed
                                },
                                async {
                                    let _ = pending.await;
                                },
                            )
                            .await;
                            Ok((closed, unrelated.await))
                        }
                        .boxed_local()
                    })
                    .await
                })
                .unwrap()
        };
        let first = fixture.pool.run_until(first_chunk).unwrap();
        assert!(exact(&first, 1 << 10), "{mode}");
        // The producer's dependency call and the unrelated call are admitted.
        fixture.wait(|fixture| {
            fixture.gates.logged("gated:start") && fixture.gates.logged("unrelated:start")
        });
        assert_eq!(fixture.calls(), 3, "{mode}: producer, gated and unrelated");
        close_now.send(()).unwrap();
        // Logical cancellation: the gated call observes its own signal while its gate is held.
        fixture.wait(|fixture| fixture.gates.logged("gated:signalled"));
        for _ in 0..50 {
            fixture.pool.run_until_stalled();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(
            !gates
                .log
                .borrow()
                .iter()
                .any(|line| line.starts_with("close:ack")),
            "{mode}: close acknowledged before the dependency physically ended: {:?}",
            gates.log.borrow()
        );
        assert!(!gates.logged("gated:end"), "{mode}");
        assert_eq!(
            fixture.calls(),
            3,
            "{mode}: still counted while physically running"
        );
        // Physical completion: only now may the close be acknowledged.
        release_physical.send(()).unwrap();
        fixture.wait(|fixture| {
            fixture
                .gates
                .log
                .borrow()
                .iter()
                .any(|line| line.starts_with("close:ack"))
        });
        assert!(
            gates.position("gated:end") < gates.position("close:ack:calls=1"),
            "{mode}: {:?}",
            gates.log.borrow()
        );
        release_unrelated.send(()).unwrap();
        let (closed, unrelated) = fixture.pool.run_until(root).unwrap();
        assert!(closed.is_ok(), "{mode}: {closed:?}");
        assert_eq!(unrelated.unwrap(), "unrelated", "{mode}");
        assert!(
            gates.logged("unrelated:end:false"),
            "{mode}: the unrelated call's signal was cancelled: {:?}",
            gates.log.borrow()
        );
        fixture.wait(|fixture| fixture.calls() == 0);
        assert_eq!(fixture.ping().unwrap(), 42, "{mode}");
        assert!(fixture.host.errors().is_empty(), "{mode}");
        fixture.close();
    });
}
