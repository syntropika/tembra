//! A source (TypeScript) export's `stream<u8>` result consumed by native Rust as a `ByteStream`.
//!
//! The Orb is a raw source bundle packaged with `Contract::source_artifact` and run by the
//! packaged interpreter (`TEMBRA_WASM_ENGINE`). Every case runs on Cranelift and on Pulley,
//! through the real host, kernel references and roots. The caller receives the stream end as soon
//! as the guest published it; the producer keeps writing in the same admitted callback, which
//! ends only after its byte pump and the host relay have ended.
use futures::{FutureExt, StreamExt, channel::oneshot, executor::LocalPool, task::LocalSpawnExt};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    rc::Rc,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tembra::{Host, Orb, Reference, Service, portable::ByteStream};
use tembra_executor_wasm::{Contract, Engine, Install, Limits};

const WIT: &str = r#"
package tembra-test:sourcestreams;

interface ledger {
  note: func(event: string) -> string;
}

interface frames {
  frames: async func(count: u32, size: u32, mode: u32) -> stream<u8>;
  ping: func(value: u32) -> u32;
}

world producer {
  import ledger;
  export frames;
}

world ledger-provider {
  export ledger;
}
"#;

const SERVICE: &str = "frames::tembra-test:sourcestreams/frames";
const LEDGER: &str = "tembra-test:sourcestreams/ledger";
/// The pipe's byte capacity (`worker::standard::PIPE_BYTES`).
const PIPE_BYTES: usize = 64 << 10;

/// Producer modes of the source fixture below.
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

/// The fixture Orb. `writeChunks` transcribes `writeChunks(chunks)` from
/// `@syntropika/tembra-sdk` (`packages/ts/sdk/src/guest.ts`): the helper returns exactly this
/// object, recognized by the interpreter through its registered symbol.
fn source(revision: u32) -> String {
    format!(
        r#"
const WRITE_CHUNKS = Symbol.for("tembra.wasi.stream.write-chunks");
function writeChunks(chunks) {{
  const iterator = () =>
    Symbol.asyncIterator in chunks ? chunks[Symbol.asyncIterator]() : chunks[Symbol.iterator]();
  return {{
    [WRITE_CHUNKS]: iterator,
    async *[Symbol.asyncIterator]() {{ for await (const chunk of chunks) yield* chunk; }},
  }};
}}
/**
 * Byte `j` of chunk `i` is `(i + j) mod 256`: a view of one pattern built while the bundle loads,
 * so producing a chunk spends no per-byte instructions of the callback's fuel (the
 * whole stream is one callback, bounded by its fuel).
 */
const PATTERN = new Uint8Array((1 << 16) + 256);
for (let offset = 0; offset < 256; offset++) PATTERN[offset] = offset;
for (let length = 256; length < PATTERN.length; length *= 2) PATTERN.copyWithin(length, 0, length);
function chunk(index, size) {{
  const start = index & 255;
  return PATTERN.subarray(start, start + size);
}}
globalThis.__tembraOrb = {{ services: {{ frames: {{
  create(self) {{
    return {{ ledger: self.get("{LEDGER}", ["main"]) }};
  }},
  methods: {{
    ping(_environment, _context, value) {{ return value + {revision}; }},
    async frames(environment, context, count, size, mode) {{
      switch (mode) {{
        case {CHUNKS}:
          return writeChunks((async function* () {{
            let index = 0;
            try {{
              for (; index < count; index++) yield chunk(index, size);
            }} finally {{
              console.log("producer:returned:" + index);
            }}
          }})());
        case {BYTES}:
          return (async function* () {{
            for (let index = 0; index < count; index++)
              for (let offset = 0; offset < size; offset++) yield (index + offset) & 255;
          }})();
        case {ARRAY}: {{
          const bytes = new Uint8Array(count * size);
          for (let index = 0; index < count; index++) bytes.set(chunk(index, size), index * size);
          return bytes;
        }}
        case {FAIL_AFTER_HEAD}:
          return writeChunks((async function* () {{
            for (let index = 0; index < count; index++) {{
              if (index === count >> 1) throw new Error("producer failed after its head");
              yield chunk(index, size);
            }}
          }})());
        case {FAIL_BEFORE_HEAD}:
          throw new Error("producer refused before its head");
        case {NOT_A_STREAM}:
          return 42;
        case {IDLE}:
          return writeChunks((async function* () {{
            try {{
              yield chunk(0, size);
              console.log("producer:idle");
              // Idle until the callback ends; one wait is bounded by the callback wait limit.
              for (;;) await tembra.wait(5000);
            }} finally {{
              console.log("producer:returned");
            }}
          }})());
        case {DEPENDENCY}:
          return writeChunks((async function* () {{
            yield chunk(0, size);
            const seen = await environment.ledger.note("after-head", context);
            console.log("ledger:" + seen);
            for (let index = 1; index < count; index++) yield chunk(index, size);
          }})());
        case {THROW_UNDEFINED}:
          return writeChunks((async function* () {{
            for (let index = 0; index < count; index++) yield chunk(index, size);
            throw undefined;
          }})());
        case {CLEANUP}:
          return writeChunks((async function* () {{
            try {{
              for (let index = 0; ; index++) yield chunk(index, size);
            }} finally {{
              const seen = await environment.ledger.note("cleanup", context);
              console.log("cleanup:" + seen);
            }}
          }})());
        case {GATED}:
          return writeChunks((async function* () {{
            yield chunk(0, size);
            const seen = await environment.ledger.note("gated", context);
            console.log("gated:" + seen);
            for (let index = 1; ; index++) yield chunk(index, size);
          }})());
        default:
          throw new Error("unknown mode");
      }}
    }},
  }},
}} }} }};
"#
    )
}

fn interpreter() -> Option<Vec<u8>> {
    let Some(path) = std::env::var_os("TEMBRA_WASM_ENGINE").filter(|path| !path.is_empty()) else {
        eprintln!("skipped: TEMBRA_WASM_ENGINE names no packaged interpreter");
        return None;
    };
    Some(std::fs::read(path).unwrap())
}

fn contract(world: &str) -> Contract {
    Contract::from_source("source-streams.wit", WIT, Some(world)).unwrap()
}

fn artifact(revision: u32) -> Vec<u8> {
    contract("producer")
        .source_artifact(&source(revision), "frames", 1)
        .unwrap()
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

fn engine(
    interpreter: &[u8],
    interpreted: bool,
    limits: Limits,
) -> (Engine, Arc<Mutex<Vec<String>>>) {
    let logs = Arc::<Mutex<Vec<String>>>::default();
    let sink = logs.clone();
    let engine = if interpreted {
        Engine::interpreted(limits)
    } else {
        Engine::new(limits)
    }
    .unwrap()
    .interpreter(interpreter)
    .unwrap()
    .on_log(move |record| sink.lock().unwrap().push(record.message.clone()));
    (engine, logs)
}

fn install(interpreter: &[u8], interpreted: bool, limits: Limits) -> Fixture {
    let (engine, logs) = engine(interpreter, interpreted, limits);
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
fn engines(body: impl Fn(&[u8], bool)) {
    let Some(interpreter) = interpreter() else {
        return;
    };
    for interpreted in [false, true] {
        body(&interpreter, interpreted);
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
                "{}: the condition never held",
                self.mode()
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
#[ignore = "requires TEMBRA_WASM_ENGINE"]
fn a_source_stream_head_arrives_while_its_producer_is_still_admitted() {
    engines(|interpreter, interpreted| {
        let mut fixture = install(interpreter, interpreted, limits(30_000));
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

/// Per-byte `AsyncIterable<number>` and a whole `Uint8Array` are the other accepted sources.
#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE"]
fn per_byte_iterables_and_byte_arrays_stream_exactly() {
    engines(|interpreter, interpreted| {
        let mut fixture = install(interpreter, interpreted, limits(30_000));
        let (bytes, array) = fixture
            .root(async move |context, reference, _| {
                let mut bytes = open(&context, &reference, 3, 1000, BYTES).await?;
                let bytes = drain(&mut bytes).await.map_err(|(_, error)| error)?;
                let mut array = open(&context, &reference, 5, 50_000, ARRAY).await?;
                let array = drain(&mut array).await.map_err(|(_, error)| error)?;
                Ok((bytes, array))
            })
            .unwrap();
        let mode = fixture.mode();
        assert_eq!(bytes.len(), 3000, "{mode}");
        assert!(exact(&bytes, 1000), "{mode}");
        assert_eq!(array.len(), 250_000, "{mode}");
        assert!(exact(&array, 50_000), "{mode}");
        fixture.wait(|fixture| fixture.calls() == 0);
        assert_eq!(fixture.ping().unwrap(), 42, "{mode}");
        fixture.close();
    });
}

/// Failing before the head, or returning something that is not a byte source, publishes no
/// stream: the call fails, and the revision is not quarantined.
#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE"]
fn a_producer_without_a_head_fails_its_call_and_keeps_its_revision() {
    engines(|interpreter, interpreted| {
        let mut fixture = install(interpreter, interpreted, limits(30_000));
        let mode = fixture.mode();
        for (producer, cause) in [
            (FAIL_BEFORE_HEAD, "producer refused before its head"),
            (
                NOT_A_STREAM,
                "a stream<u8> value requires an AsyncIterable<number> source",
            ),
        ] {
            let error = fixture
                .root(async move |context, reference, _| {
                    open(&context, &reference, 1, 1, producer).await.map(drop)
                })
                .unwrap_err();
            assert!(
                !error.message.contains("quarantined"),
                "{mode} {producer}: {error:?}"
            );
            assert!(
                error.message.contains(cause),
                "{mode} {producer}: {error:?}"
            );
            fixture.wait(|fixture| fixture.calls() == 0);
            assert_eq!(fixture.ping().unwrap(), 42, "{mode} {producer}");
        }
        fixture.close();
    });
}

/// An author error after the head is never a clean end: a prefix, then an error. It is an
/// ordinary guest error, not a trap: the revision keeps serving.
#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE"]
fn an_author_error_after_the_head_fails_the_read_and_never_ends_cleanly() {
    engines(|interpreter, interpreted| {
        let mut fixture = install(interpreter, interpreted, limits(30_000));
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
        assert!(
            error.message.contains("producer failed after its head"),
            "{mode}: {error:?}"
        );
        assert!(!error.message.contains("quarantined"), "{mode}: {error:?}");
        fixture.wait(|fixture| fixture.calls() == 0);
        // An ordinary guest error, not a trap: the revision keeps serving.
        assert_eq!(fixture.ping().unwrap(), 42, "{mode}");
        // The physical failure after the head is recorded once, like a compiled producer's.
        let errors = fixture.host.errors();
        assert_eq!(errors.len(), 1, "{mode}: {errors:?}");
        assert!(
            errors[0].message.contains("producer failed after its head"),
            "{mode}: {errors:?}"
        );
        fixture.close();
    });
}

/// After its head, the producer calls a dependency under the caller's root and principal.
#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE"]
fn a_post_head_dependency_call_keeps_the_callers_root_and_principal() {
    engines(|interpreter, interpreted| {
        let mut fixture = install(interpreter, interpreted, limits(30_000));
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
#[ignore = "requires TEMBRA_WASM_ENGINE"]
fn dropping_a_backpressured_stream_stops_its_producer() {
    engines(|interpreter, interpreted| {
        let mut fixture = install(interpreter, interpreted, limits(30_000));
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
/// at once; it does not wait for the revision's timeout.
#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE"]
fn dropping_a_stream_of_an_idle_producer_ends_its_callback() {
    engines(|interpreter, interpreted| {
        let mut fixture = install(interpreter, interpreted, limits(30_000));
        let logs = fixture.logs.clone();
        let (first, closing) = fixture
            .root(async move |context, reference, _| {
                let mut stream = open(&context, &reference, 2, 1 << 10, IDLE).await?;
                let first = stream.next().await.unwrap()?;
                // Observe the producer's own idle marker before measuring its cancellation.
                let deadline = Instant::now() + Duration::from_secs(20);
                while !logs
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|line| line == "producer:idle")
                {
                    assert!(Instant::now() < deadline, "the producer never became idle");
                    futures_timer::Delay::new(Duration::from_millis(2)).await;
                }
                // Opening and reading the stream are preparation, not close latency.
                let closing = Instant::now();
                drop(stream);
                Ok((first, closing))
            })
            .unwrap();
        let mode = fixture.mode();
        assert!(exact(&first, 1 << 10), "{mode}");
        fixture.wait(|fixture| fixture.calls() == 0);
        let elapsed = closing.elapsed();
        assert!(
            elapsed < Duration::from_secs(10),
            "{mode}: close took {elapsed:?}"
        );
        assert!(
            fixture.logged("producer:returned"),
            "{mode}: cleanup did not run"
        );
        assert_eq!(fixture.ping().unwrap(), 42, "{mode}");
        let errors = fixture.host.errors();
        assert!(errors.is_empty(), "{mode}: {errors:?}");
        fixture.close();
    });
}

/// Cancelling the root of an idle producer fails the read; cancellation is not a trap.
#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE"]
fn cancelling_the_root_fails_the_read_and_keeps_the_revision() {
    engines(|interpreter, interpreted| {
        let mut fixture = install(interpreter, interpreted, limits(30_000));
        let (first, outcome) = fixture
            .root(async move |context, reference, _| {
                let mut stream = open(&context, &reference, 2, 1 << 10, IDLE).await?;
                let first = stream.next().await.unwrap()?;
                context.cancel()?;
                Ok((first, drain(&mut stream).await))
            })
            .unwrap();
        let mode = fixture.mode();
        assert!(exact(&first, 1 << 10), "{mode}");
        let Err((rest, error)) = outcome else {
            panic!("{mode}: a cancelled producer ended cleanly");
        };
        assert!(rest.is_empty(), "{mode}");
        assert!(!error.message.contains("quarantined"), "{mode}: {error:?}");
        fixture.wait(|fixture| fixture.calls() == 0);
        assert_eq!(fixture.ping().unwrap(), 42, "{mode}");
        eprintln!(
            "{mode}: cancelled read: {error:?}; host errors {:?}",
            fixture.host.errors()
        );
        fixture.close();
    });
}

/// The whole callback, stream included, is bounded by the revision timeout. Expiry is terminal
/// (the selected policy): the read fails and the revision is quarantined.
#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE"]
fn the_terminal_timeout_fails_the_read_and_quarantines_the_revision() {
    engines(|interpreter, interpreted| {
        let mut fixture = install(interpreter, interpreted, limits(1_500));
        let outcome = fixture
            .root(async move |context, reference, _| {
                let mut stream = open(&context, &reference, 2, 1 << 10, IDLE).await?;
                Ok(drain(&mut stream).await)
            })
            .unwrap();
        let mode = fixture.mode();
        let Err((prefix, error)) = outcome else {
            panic!("{mode}: a timed-out producer ended cleanly");
        };
        assert_eq!(prefix.len(), 1 << 10, "{mode}");
        // The dropped Store fails the pipe, or the settlement reports the wall time first.
        assert!(
            error.message.contains("wall time")
                || error
                    .message
                    .contains("ended before its stream was delivered"),
            "{mode}: {error:?}"
        );
        fixture.wait(|fixture| fixture.calls() == 0);
        let refused = fixture.ping().unwrap_err();
        assert_eq!(
            refused.reason, "wasm_executor_quarantined",
            "{mode}: {refused:?}"
        );
        assert!(refused.message.contains("wall time"), "{mode}: {refused:?}");
        let _ = fixture.host.errors();
        fixture.close();
    });
}

/// A reload while a stream is being read lets the old revision finish it; new calls reach the
/// replacement, and closing the host releases everything.
#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE"]
fn a_reload_lets_an_open_stream_finish_on_its_revision() {
    engines(|interpreter, interpreted| {
        let mut fixture = install(interpreter, interpreted, limits(30_000));
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
        let replacement = fixture
            .engine
            .load(
                &artifact(2),
                Install::default().requires(LEDGER, "ledger", 1),
            )
            .unwrap();
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

/// A packet limit far below the 64 KiB write chunk does not turn a stream into a clean end.
#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE"]
fn a_small_packet_limit_never_truncates_a_stream_into_a_clean_end() {
    engines(|interpreter, interpreted| {
        let mut fixture = install(
            interpreter,
            interpreted,
            Limits {
                packet_bytes: 16 << 10,
                ..limits(30_000)
            },
        );
        let outcome = fixture
            .root(async move |context, reference, _| {
                let mut stream = open(&context, &reference, 4, 64 << 10, CHUNKS).await?;
                Ok(drain(&mut stream).await)
            })
            .unwrap();
        let mode = fixture.mode();
        let Err((prefix, error)) = outcome else {
            panic!("{mode}: an oversized binary packet was accepted");
        };
        assert!(
            prefix.is_empty(),
            "{mode}: bytes accepted before the limit check"
        );
        eprintln!("{mode}: small packet limit failed the stream: {error:?}");
        fixture.wait(|fixture| fixture.calls() == 0);
        let errors = fixture.host.errors();
        assert_eq!(errors.len(), 1, "{mode}: {errors:?}");
        assert!(
            errors[0].message.contains("packet limit"),
            "{mode}: {errors:?}"
        );
        assert_eq!(fixture.ping().unwrap(), 42, "{mode}");
        fixture.close();
    });
}

/// Data-only platform ports keep refusing a source stream export until their carrier exists.
#[test]
fn platform_data_ports_refuse_source_stream_exports() {
    let engine = Engine::new(Limits::default()).unwrap();
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
    assert!(error.message.contains("stream"), "{error:?}");
}

/// Data-only platform ports keep refusing stream imports until their carrier exists.
#[test]
fn platform_data_ports_refuse_source_stream_imports() {
    const CONSUMER: &str = r#"
package tembra-test:sourceimports;

interface frames {
  frames: async func(count: u32) -> stream<u8>;
}

interface total {
  total: async func() -> u32;
}

world consumer {
  import frames;
  export total;
}
"#;
    let artifact = Contract::from_source("source-imports.wit", CONSUMER, Some("consumer"))
        .unwrap()
        .source_artifact("globalThis.__tembraOrb={services:{}}", "consumer", 1)
        .unwrap();
    let artifact = tembra_contracts::Artifact::decode(&artifact).unwrap();
    let bindings = artifact
        .contract
        .providers(
            &BTreeMap::from([(
                "tembra-test:sourceimports/frames".to_owned(),
                tembra_contracts::OrbProvider {
                    orb: "frames".into(),
                    api_version: 1,
                },
            )]),
            &Default::default(),
        )
        .unwrap();
    let engine = Engine::new(Limits::default()).unwrap();
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

/// A source may throw any value: `throw undefined` after its head is a failure, never a clean
/// end of the bytes it produced.
#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE"]
fn a_producer_throwing_undefined_fails_the_read() {
    engines(|interpreter, interpreted| {
        let mut fixture = install(interpreter, interpreted, limits(30_000));
        let outcome = fixture
            .root(async move |context, reference, _| {
                let mut stream = open(&context, &reference, 4, 64 << 10, THROW_UNDEFINED).await?;
                Ok(drain(&mut stream).await)
            })
            .unwrap();
        let mode = fixture.mode();
        let Err((prefix, _)) = outcome else {
            panic!("{mode}: a producer throwing undefined ended cleanly");
        };
        assert!(prefix.len() <= 4 << 16, "{mode}");
        assert!(exact(&prefix, 64 << 10), "{mode}");
        fixture.wait(|fixture| fixture.calls() == 0);
        assert_eq!(fixture.ping().unwrap(), 42, "{mode}");
        assert_eq!(fixture.host.errors().len(), 1, "{mode}");
        fixture.close();
    });
}

/// After the caller closes the stream, the producer callback ends without awaiting the source's
/// asynchronous cleanup (`return()`), so that cleanup cannot perform host operations: a
/// dependency call issued from its `finally` once the callback ended is never admitted (both
/// engines observe none). A dependency call admitted before the callback ended is cancelled and
/// joined instead (`closing_a_stream_cancels_and_joins_its_admitted_dependency`). Either way
/// nothing outlives the call, and the close is acknowledged after physical settlement.
#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE"]
fn closing_a_stream_never_leaves_its_cleanup_detached() {
    engines(|interpreter, interpreted| {
        let mut fixture = install(interpreter, interpreted, limits(30_000));
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
        assert!(closed.is_ok(), "{mode}: {closed:?}");
        assert_eq!(
            fixture.calls(),
            0,
            "{mode}: acknowledged before physical settlement"
        );
        let seen = fixture.seen.borrow().clone();
        assert!(
            seen.is_empty() || seen.iter().all(|(event, ..)| event == "cleanup"),
            "{mode}: {seen:?}"
        );
        eprintln!(
            "{mode}: cleanup dependency calls completed before the close: {}",
            seen.len()
        );
        assert_eq!(fixture.ping().unwrap(), 42, "{mode}");
        assert!(fixture.host.errors().is_empty(), "{mode}");
        fixture.close();
    });
}

/// B1: closing a stream while its producer awaits an admitted dependency call cancels exactly
/// that call and joins it. The call observes its own cancellation (logical) while its physical
/// gate is held; the close is not acknowledged until the call has physically ended. An
/// unrelated call of the consumer's own root keeps a live signal throughout.
#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE"]
fn closing_a_stream_cancels_and_joins_its_admitted_dependency() {
    engines(|interpreter, interpreted| {
        let mut fixture = install(interpreter, interpreted, limits(30_000));
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
