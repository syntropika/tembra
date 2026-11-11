//! A source (TypeScript) Orb consuming the whole `stream<u8>` result of an imported Orb.
//!
//! The consumer is a raw source bundle run by the packaged interpreter (`TEMBRA_WASM_ENGINE`),
//! loaded through `Engine::load` (the native facade). Its producer is a native Orb, a source Orb,
//! or the compiled S2 producer (`TEMBRA_WASM_STREAM_PRODUCER`). Every case runs on Cranelift and
//! on Pulley, through the real host, kernel references and roots.
//!
//! The consumer imports no WASI interface and receives no grant: its stream reader comes from
//! the standard bootstrap, which a contract defining a stream loads without any WASI module. The
//! import's request is the ordinary `reference.call` packet on the `STREAM_IMPORT` carrier; the
//! host pumps the delivered end into a bounded pipe of the consumer's Store within its callback.
use futures::{FutureExt, channel::oneshot, executor::LocalPool, task::LocalSpawnExt};
use serde_json::{Value, json};
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

const WIT: &str = r#"
package tembra-test:streams@1.0.0;

interface frames {
    frames: async func(count: u32, size: u32, mode: u32) -> stream<u8>;
    ping: func(value: u32) -> u32;
}

interface aliased {
    type bytes = stream<u8>;
    frames: async func(count: u32, size: u32, mode: u32) -> bytes;
    ping: func(value: u32) -> u32;
}

interface reader {
    read: async func(count: u32, size: u32, mode: u32, take: u64, style: u32) -> tuple<u64, u32, u32>;
    failure: async func(count: u32, size: u32, mode: u32) -> tuple<u64, string>;
    ignore: async func(count: u32, size: u32, mode: u32) -> u32;
    closing: async func(count: u32, size: u32) -> u32;
    many: async func(n: u32, count: u32, size: u32) -> u64;
    abandon: async func(count: u32, size: u32) -> u32;
    forge: async func(which: u32) -> string;
    ping: func(value: u32) -> u32;
}

interface relayed {
    frames: async func(count: u32, size: u32, mode: u32) -> stream<u8>;
}

world consumer-orb {
    import frames;
    import aliased;
    export reader;
}

world relay-orb {
    import frames;
    export relayed;
}

world producer-orb {
    export frames;
}

world aliased-orb {
    export aliased;
}
"#;

const FRAMES: &str = "tembra-test:streams/frames@1.0.0";
const ALIASED: &str = "tembra-test:streams/aliased@1.0.0";
const READER: &str = "reader::tembra-test:streams/reader@1.0.0";
const RELAY: &str = "relay::tembra-test:streams/relayed@1.0.0";
const PRODUCER: &str = "frames::tembra-test:streams/frames@1.0.0";
/// The pipe's byte capacity (`worker::standard::PIPE_BYTES`).
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
/// Native only: wait for the call's own cancellation and then a physical gate, then answer.
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

/// `JSON.stringify` of a `reference.call` request is ASCII here, so the forging helpers below
/// encode and decode it byte for byte.
fn consumer_source(revision: u32) -> String {
    format!(
        r#"
const READ_CHUNK = Symbol.for("tembra.wasi.stream.read-chunk");
const DISPOSE = Symbol.dispose ?? Symbol.for("Symbol.dispose");
/** `readChunks` of `@syntropika/tembra-sdk` (`packages/ts/sdk/src/guest.ts`), interpreter path. */
async function* readChunks(stream) {{
  const read = stream[READ_CHUNK];
  if (typeof read !== "function") throw new Error("not an interpreter byte stream");
  try {{
    for (let chunk = await read.call(stream); chunk; chunk = await read.call(stream)) yield chunk;
  }} finally {{
    stream[DISPOSE]?.();
  }}
}}
/** Byte `j` of chunk `i` is `(i + j) mod 256`; count mismatches while advancing `at`. */
function check(chunk, at, size) {{
  let bad = 0;
  for (let k = 0; k < chunk.length; k++) {{
    if (chunk[k] !== ((at.index + at.offset) & 255)) bad++;
    if (++at.offset === size) {{
      at.offset = 0;
      at.index++;
    }}
  }}
  return bad;
}}
function message(error) {{
  return error instanceof Error ? error.message : "thrown " + String(error);
}}
function ascii(text) {{
  const bytes = new Uint8Array(text.length);
  for (let at = 0; at < text.length; at++) bytes[at] = text.charCodeAt(at);
  return bytes;
}}
/** A binary `STREAM_IMPORT` packet carrying `json`, plus `extra` trailing bytes. */
function streamPacket(json, extra = 0) {{
  const body = ascii(json);
  const packet = new Uint8Array(6 + body.length + extra);
  packet[1] = 10;
  new DataView(packet.buffer).setUint32(2, body.length, true);
  packet.set(body, 6);
  return packet;
}}
function streamJson(packet) {{
  const length = new DataView(packet.buffer, packet.byteOffset).getUint32(2, true);
  let text = "";
  for (let at = 0; at < length; at++) text += String.fromCharCode(packet[6 + at]);
  return text;
}}
/** Rewrite the next host request once, then restore the real carrier. */
function intercept(rewrite) {{
  const original = globalThis.__hostAsync;
  globalThis.__hostAsync = (packet) => {{
    globalThis.__hostAsync = original;
    return original(rewrite(packet, original));
  }};
}}
async function refused(run) {{
  try {{
    await run();
    return "accepted";
  }} catch (error) {{
    return message(error);
  }}
}}
globalThis.__tembraOrb = {{ services: {{ reader: {{
  create(self) {{
    return {{
      frames: self.get("{FRAMES}", ["main"]),
      aliased: self.get("{ALIASED}", ["main"]),
    }};
  }},
  methods: {{
    ping(_environment, _context, value) {{ return value + {revision}; }},
    async read(environment, context, count, size, mode, take, style) {{
      const source = style >= {ALIAS} ? environment.aliased : environment.frames;
      const how = style % {ALIAS};
      const limit = take > BigInt(Number.MAX_SAFE_INTEGER) ? Infinity : Number(take);
      const stream = await source.frames(count, size, mode, context);
      const at = {{ index: 0, offset: 0 }};
      let bytes = 0, bad = 0, reads = 0;
      if (how === {BYTES}) {{
        for await (const byte of stream) {{
          if (byte !== ((at.index + at.offset) & 255)) bad++;
          if (++at.offset === size) {{
            at.offset = 0;
            at.index++;
          }}
          if (++bytes >= limit) break;
        }}
      }} else {{
        for await (const chunk of readChunks(stream)) {{
          reads++;
          bad += check(chunk, at, size);
          bytes += chunk.length;
          if (how === {PROBED} && reads === 1) await source.ping({PROBE}, context);
          if (bytes >= limit) break;
        }}
      }}
      return [BigInt(bytes), bad, reads];
    }},
    async failure(environment, context, count, size, mode) {{
      let stream;
      try {{
        stream = await environment.frames.frames(count, size, mode, context);
      }} catch (error) {{
        return [0n, "import:" + message(error)];
      }}
      const at = {{ index: 0, offset: 0 }};
      let bytes = 0, bad = 0;
      try {{
        for await (const chunk of readChunks(stream)) {{
          bad += check(chunk, at, size);
          bytes += chunk.length;
        }}
      }} catch (error) {{
        return [BigInt(bytes), (bad ? "corrupt:" : "read:") + message(error)];
      }}
      return [BigInt(bytes), bad ? "corrupt" : "end"];
    }},
    async ignore(environment, context, count, size, mode) {{
      await environment.frames.frames(count, size, mode, context);
      return 7;
    }},
    async closing(environment, context, count, size) {{
      const stream = await environment.frames.frames(count, size, {QUIET}, context);
      const first = await stream[READ_CHUNK]();
      if (first?.length !== size) throw new Error("the first chunk is missing");
      stream[DISPOSE]();
      // Answered only once the producer saw its stream closed, so this callback is still
      // running when the close reaches it.
      return await environment.frames.ping({CLOSED}, context);
    }},
    async many(environment, context, n, count, size) {{
      const streams = await Promise.all(
        Array.from({{ length: n }}, () => environment.frames.frames(count, size, {CLEAN}, context)),
      );
      let total = 0;
      for (const stream of streams)
        for await (const chunk of readChunks(stream)) total += chunk.length;
      return BigInt(total);
    }},
    async abandon(environment, context, count, size) {{
      const pending = environment.frames.frames(count, size, {GATED}, context);
      pending.then((stream) => stream[DISPOSE]?.(), () => {{}});
      // Answered once the gated call runs; the callback then ends with its import pending.
      return await environment.frames.ping({STARTED}, context);
    }},
    async forge(environment, context, which) {{
      const frames = environment.frames;
      switch (which) {{
        case 0:
          // A scalar import call on the stream carrier.
          intercept((packet) => streamPacket(packet));
          return refused(() => frames.ping(1, context));
        case 1:
          // A stream import as an ordinary JSON request.
          intercept((packet) => streamJson(packet));
          return refused(() => frames.frames(1, 1, {CLEAN}, context));
        case 2:
          // Another host operation on the stream carrier.
          intercept(() => streamPacket(JSON.stringify({{ operation: "wait", milliseconds: 1 }})));
          return refused(() => frames.frames(1, 1, {CLEAN}, context));
        case 3:
          // Trailing bytes after the request.
          intercept((packet) => streamPacket(streamJson(packet), 1));
          return refused(() => frames.frames(1, 1, {CLEAN}, context));
        case 4: {{
          // Arguments outside the import's signature.
          intercept((packet) => {{
            const request = JSON.parse(streamJson(packet));
            request.args = ["one", 1, 0];
            return streamPacket(JSON.stringify(request));
          }});
          return refused(() => frames.frames(1, 1, {CLEAN}, context));
        }}
        case 5: {{
          // A handle this environment never issued.
          intercept((packet) => {{
            const request = JSON.parse(streamJson(packet));
            request.id = 999999;
            return streamPacket(JSON.stringify(request));
          }});
          return refused(() => frames.frames(1, 1, {CLEAN}, context));
        }}
        case 6:
          // An operation the standard carrier does not have.
          intercept(() => Uint8Array.of(0, 11));
          return refused(() => frames.frames(1, 1, {CLEAN}, context));
        default:
          throw new Error("unknown forgery");
      }}
    }},
  }},
}} }} }};
"#
    )
}

/// The source producer; `writeChunks` transcribes `@syntropika/tembra-sdk`'s helper.
fn producer_source() -> String {
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
const PATTERN = new Uint8Array((1 << 16) + 256);
for (let offset = 0; offset < 256; offset++) PATTERN[offset] = offset;
for (let length = 256; length < PATTERN.length; length *= 2) PATTERN.copyWithin(length, 0, length);
function chunk(index, size) {{
  const start = index & 255;
  return PATTERN.subarray(start, start + size);
}}
globalThis.__tembraOrb = {{ services: {{ frames: {{
  methods: {{
    ping(_environment, _context, value) {{ return value + 1; }},
    async frames(_environment, _context, count, size, mode) {{
      switch (mode) {{
        case {CLEAN}:
          return writeChunks((async function* () {{
            let index = 0;
            try {{
              for (; index < count; index++) yield chunk(index, size);
            }} finally {{
              console.log("producer:returned:" + index);
            }}
          }})());
        case {FAIL_AFTER}:
          return writeChunks((async function* () {{
            for (let index = 0; index < count; index++) {{
              if (index === count >> 1) throw new Error("source producer failed after its head");
              yield chunk(index, size);
            }}
          }})());
        case {QUIET}:
          return writeChunks((async function* () {{
            try {{
              yield chunk(0, size);
              console.log("producer:quiet");
              for (;;) await tembra.wait(5000);
            }} finally {{
              console.log("producer:returned");
            }}
          }})());
        case {FAIL_BEFORE}:
          throw new Error("source producer refused before its head");
        case {THROW_UNDEFINED}:
          return writeChunks((async function* () {{
            for (let index = 0; index < count; index++) yield chunk(index, size);
            throw undefined;
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

/// A source Orb forwarding its imported stream as its own whole result.
fn relay_source() -> String {
    format!(
        r#"
globalThis.__tembraOrb = {{ services: {{ relayed: {{
  create(self) {{
    return {{ frames: self.get("{FRAMES}", ["main"]) }};
  }},
  methods: {{
    async frames(environment, context, count, size, mode) {{
      return await environment.frames.frames(count, size, mode, context);
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

fn compiled_producer() -> Option<Vec<u8>> {
    let Some(path) =
        std::env::var_os("TEMBRA_WASM_STREAM_PRODUCER").filter(|path| !path.is_empty())
    else {
        eprintln!("skipped: TEMBRA_WASM_STREAM_PRODUCER names no compiled stream producer");
        return None;
    };
    Some(std::fs::read(path).unwrap())
}

fn contract(world: &str) -> Contract {
    Contract::from_source("streams.wit", WIT, Some(world)).unwrap()
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
                            context.signal().cancelled().await;
                            native.note("gated:signalled");
                            let physical = native.physical.borrow_mut().take();
                            if let Some(physical) = physical {
                                let _ = physical.await;
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

fn consumer(engine: &Engine, revision: u32) -> Orb {
    engine
        .load(
            &contract("consumer-orb")
                .source_artifact(&consumer_source(revision), "reader", 1)
                .unwrap(),
            Install::default()
                .requires(FRAMES, "frames", 1)
                .requires(ALIASED, "aliased", 1),
        )
        .unwrap()
}

fn relay(engine: &Engine) -> Orb {
    engine
        .load(
            &contract("relay-orb")
                .source_artifact(&relay_source(), "relay", 1)
                .unwrap(),
            Install::default().requires(FRAMES, "frames", 1),
        )
        .unwrap()
}

fn producer(engine: &Engine, kind: Kind, native: &Rc<Native>, compiled: Option<&[u8]>) -> Orb {
    match kind {
        Kind::Native => self::native(native.clone(), "frames", "producer-orb"),
        Kind::Source => engine
            .load(
                &contract("producer-orb")
                    .source_artifact(&producer_source(), "frames", 1)
                    .unwrap(),
                Install::default(),
            )
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

fn install(
    interpreter: &[u8],
    interpreted: bool,
    limits: Limits,
    kind: Kind,
    compiled: Option<&[u8]>,
) -> Fixture {
    let (engine, logs) = engine(interpreter, interpreted, limits);
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
fn engines(body: impl Fn(&[u8], bool)) {
    let Some(interpreter) = interpreter() else {
        return;
    };
    for interpreted in [false, true] {
        body(&interpreter, interpreted);
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
#[ignore = "requires TEMBRA_WASM_ENGINE"]
fn a_source_consumer_reads_native_and_source_producers_exactly() {
    engines(|interpreter, interpreted| {
        for kind in [Kind::Native, Kind::Source] {
            let mut fixture = install(interpreter, interpreted, limits(30_000), kind, None);
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
#[ignore = "requires TEMBRA_WASM_ENGINE and TEMBRA_WASM_STREAM_PRODUCER"]
fn a_source_consumer_reads_a_compiled_producer_exactly_and_sees_its_trap() {
    let Some(compiled) = compiled_producer() else {
        return;
    };
    engines(|interpreter, interpreted| {
        let mut fixture = install(
            interpreter,
            interpreted,
            limits(30_000),
            Kind::Compiled,
            Some(&compiled),
        );
        reads_exactly(&mut fixture);
        // The compiled producer traps after its whole body while holding its writer: the
        // consumer never sees a clean end.
        let mode = fixture.mode();
        let (bytes, outcome) = fixture
            .call::<_, (u64, String)>("failure", (4_u32, 1024_u32, FAIL_AFTER))
            .unwrap();
        assert!(bytes <= 4096, "{mode}: {bytes}");
        assert!(outcome.starts_with("read:"), "{mode}: {outcome}");
        fixture.wait(|fixture| fixture.calls() == 0);
        assert_eq!(
            fixture.call::<_, u32>("ping", (41_u32,)).unwrap(),
            42,
            "{mode}"
        );
        let _ = fixture.host.errors();
        fixture.close();
    });
}

/// The head reaches the consumer before its producer finished, and the producer runs ahead only
/// by the bounded buffers.
#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE"]
fn the_head_arrives_early_and_the_producer_is_backpressured() {
    engines(|interpreter, interpreted| {
        let mut fixture = install(interpreter, interpreted, limits(30_000), Kind::Native, None);
        let mode = fixture.mode();
        // Three pipes exceed the allowed buffering without making this backpressure check a
        // large interpreted byte-validation workload; complete reads are qualified separately.
        let (count, size) = (48_u32, 4096_u32);
        let (bytes, mismatches, _) = fixture.read(count, size, CLEAN, u64::MAX, PROBED).unwrap();
        assert_eq!((bytes, mismatches), (u64::from(count * size), 0), "{mode}");
        let probed = fixture.native.probed.get().expect("probed");
        // The consumer's first read (at most one pipe), the refilled pipe, the chunk the pump
        // holds and the one in flight: still below the whole three-pipe body.
        assert!(
            probed * size as usize <= 2 * PIPE_BYTES + 2 * size as usize,
            "{mode}: {probed}"
        );
        assert!(probed < count as usize, "{mode}");
        fixture.settled();
        fixture.close();
    });
}

/// A native caller reads a source relay forwarding another source Orb's (or a native Orb's)
/// stream: exact bytes, a failure that never ends cleanly, and a close that reaches the origin.
#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE"]
fn a_source_relay_forwards_a_stream_from_source_or_native_to_native() {
    engines(|interpreter, interpreted| {
        for kind in [Kind::Source, Kind::Native] {
            let mut fixture = install(interpreter, interpreted, limits(30_000), kind, None);
            let mode = fixture.mode();
            fixture.host.install(vec![relay(&fixture.engine)]).unwrap();
            let relayed = Rc::new(
                fixture
                    .host
                    .get(RELAY, vec!["main".into()], "alice")
                    .unwrap(),
            );
            fixture.pool.run_until(relayed.ready()).unwrap();
            let read = |fixture: &mut Fixture, mode: u32, take: usize| {
                let relayed = relayed.clone();
                fixture
                    .pool
                    .run_until(fixture.host.root("alice", move |context| {
                        async move {
                            let mut stream = relayed
                                .call_portable::<_, ByteStream>(
                                    &context,
                                    "frames",
                                    (COUNT, SIZE, mode),
                                )
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
            };
            let (received, failure) = read(&mut fixture, CLEAN, usize::MAX).unwrap();
            assert!(failure.is_none(), "{mode}: {failure:?}");
            assert_eq!(received.len(), (COUNT * SIZE) as usize, "{mode}");
            assert!(exact(&received, SIZE as usize), "{mode}");
            // A failure after the origin's head reaches the native reader as an error.
            let (received, failure) = read(&mut fixture, FAIL_AFTER, usize::MAX).unwrap();
            assert!(failure.is_some(), "{mode}: a failed origin ended cleanly");
            assert!(received.len() <= (COUNT / 2 * SIZE) as usize, "{mode}");
            assert!(exact(&received, SIZE as usize), "{mode}");
            // A failure before the origin's head fails the relayed call.
            let error = read(&mut fixture, FAIL_BEFORE, usize::MAX).unwrap_err();
            assert!(
                error.message.contains("refused before its head"),
                "{mode}: {error:?}"
            );
            // Closing after one chunk reaches the origin, which stops.
            let closes = fixture.native.closes.get();
            let (received, failure) = read(&mut fixture, QUIET, 1).unwrap();
            assert!(
                failure.is_none() && received.len() == SIZE as usize,
                "{mode}"
            );
            if kind == Kind::Native {
                // The quiet origin was closed; every origin with a head ended physically.
                fixture.wait(|fixture| {
                    fixture.native.closes.get() > closes && fixture.native.ended.get() == 3
                });
            } else {
                fixture.wait(|fixture| fixture.logged("producer:returned"));
            }
            fixture.wait(|fixture| fixture.calls() == 0);
            relayed.release().unwrap();
            // The failed origin's error is recorded by the host, not swallowed.
            let errors = fixture.host.errors();
            assert!(
                kind == Kind::Native
                    || errors
                        .iter()
                        .any(|error| error.message.contains("failed after its head")),
                "{mode}: {errors:?}"
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
#[ignore = "requires TEMBRA_WASM_ENGINE"]
fn returning_early_or_closing_stops_the_producer() {
    engines(|interpreter, interpreted| {
        let mut fixture = install(interpreter, interpreted, limits(30_000), Kind::Native, None);
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
        let mut fixture = install(interpreter, interpreted, limits(30_000), Kind::Source, None);
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

/// A producer failure before the head rejects the import; after the head it fails the read
/// after an exact prefix. Neither is ever a clean end.
#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE"]
fn producer_failures_are_errors_before_and_after_the_head() {
    engines(|interpreter, interpreted| {
        for kind in [Kind::Native, Kind::Source] {
            let mut fixture = install(interpreter, interpreted, limits(30_000), kind, None);
            let mode = fixture.mode();
            let mut failure = |producer: u32| {
                fixture
                    .call::<_, (u64, String)>("failure", (4_u32, 1024_u32, producer))
                    .unwrap()
            };
            let (bytes, outcome) = failure(FAIL_BEFORE);
            assert_eq!(bytes, 0, "{mode}");
            assert!(
                outcome.starts_with("import:") && outcome.contains("refused before its head"),
                "{mode}: {outcome}"
            );
            let (bytes, outcome) = failure(FAIL_AFTER);
            assert!(bytes <= 2048, "{mode}: {bytes}");
            assert!(outcome.starts_with("read:"), "{mode}: {outcome}");
            if kind == Kind::Native {
                assert!(
                    outcome.contains("failed after its head"),
                    "{mode}: {outcome}"
                );
            }
            if kind == Kind::Source {
                let (bytes, outcome) = failure(THROW_UNDEFINED);
                assert!(bytes <= 4096, "{mode}: {bytes}");
                assert!(outcome.starts_with("read:"), "{mode}: {outcome}");
                // Both failed producer callbacks are recorded by the host.
                let errors = fixture.host.errors();
                assert!(
                    errors
                        .iter()
                        .any(|error| error.message.contains("failed after its head")),
                    "{mode}: {errors:?}"
                );
            }
            fixture.settled();
            assert_eq!(fixture.producer_ping().unwrap(), 2, "{mode}");
            fixture.close();
        }
    });
}

/// Root cancellation ends the import without quarantine; the callback timeout is terminal for
/// the consumer. Either way the producer is closed and physically ends.
#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE"]
fn cancellation_and_timeout_close_the_producer() {
    engines(|interpreter, interpreted| {
        let mut fixture = install(interpreter, interpreted, limits(30_000), Kind::Native, None);
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
        let error = outcome.unwrap().unwrap_err();
        assert_ne!(
            error.reason, "wasm_executor_quarantined",
            "{mode}: {error:?}"
        );
        fixture.wait(|fixture| fixture.native.closes.get() == 1 && fixture.native.ended.get() == 1);
        fixture.settled();
        fixture.close();

        let mut fixture = install(interpreter, interpreted, limits(1_500), Kind::Native, None);
        let mode = fixture.mode();
        let started = Instant::now();
        let error = fixture.read(4, 1024, QUIET, u64::MAX, CHUNKS).unwrap_err();
        assert!(started.elapsed() >= Duration::from_millis(1_400), "{mode}");
        assert!(error.message.contains("wall time"), "{mode}: {error:?}");
        fixture.wait(|fixture| {
            fixture.native.closes.get() == 1
                && fixture.native.ended.get() == 1
                && fixture.calls() == 0
        });
        let refused = fixture.call::<_, u32>("ping", (1_u32,)).unwrap_err();
        assert_eq!(
            refused.reason, "wasm_executor_quarantined",
            "{mode}: {refused:?}"
        );
        assert_eq!(fixture.producer_ping().unwrap(), 2, "{mode}");
        let _ = fixture.host.errors();
        fixture.close();
    });
}

/// A reload while the consumer reads lets the stream finish on the old revision; new calls reach
/// the replacement.
#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE"]
fn a_reload_lets_an_open_import_finish_on_its_revision() {
    engines(|interpreter, interpreted| {
        let mut fixture = install(interpreter, interpreted, limits(30_000), Kind::Native, None);
        let mode = fixture.mode();
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
        let replacement = consumer(&fixture.engine, 2);
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

/// A forged request is refused before anything is called: a data import on the stream carrier,
/// a stream import as a data request, another operation, a malformed packet, invalid arguments
/// or a foreign handle.
#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE"]
fn forged_stream_requests_are_refused_before_any_call() {
    engines(|interpreter, interpreted| {
        let mut fixture = install(interpreter, interpreted, limits(30_000), Kind::Native, None);
        let mode = fixture.mode();
        for (which, expected) in [
            (0, "does not return a byte stream"),
            (1, "called only on its stream carrier"),
            (2, "carries only an import call"),
            (3, "trailing bytes"),
            (4, "does not match U32"),
            (5, "handle"),
            (6, "unknown standard operation"),
        ] {
            let refusal = fixture.call::<_, String>("forge", (which,)).unwrap();
            assert!(refusal.contains(expected), "{mode} {which}: {refusal}");
            assert!(fixture.native.calls.borrow().is_empty(), "{mode} {which}");
            assert_eq!(fixture.native.pings.get(), 0, "{mode} {which}");
        }
        fixture.settled();
        assert!(fixture.native.calls.borrow().is_empty(), "{mode}");
        fixture.close();
    });
}

/// Sixteen imports may be outstanding at once; the seventeenth is refused, the delivered ends
/// are closed, and the revision keeps serving.
#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE"]
fn outstanding_stream_imports_are_bounded() {
    engines(|interpreter, interpreted| {
        let mut fixture = install(interpreter, interpreted, limits(30_000), Kind::Native, None);
        let mode = fixture.mode();
        let total: u64 = fixture.call("many", (16_u32, 20_u32, 4096_u32)).unwrap();
        assert_eq!(total, 16 * 20 * 4096, "{mode}");
        let closes = fixture.native.closes.get();
        let error = fixture
            .call::<_, u64>("many", (17_u32, 20_u32, 4096_u32))
            .unwrap_err();
        assert!(
            error.message.contains("stream imports outstanding"),
            "{mode}: {error:?}"
        );
        assert_ne!(error.reason, "wasm_executor_quarantined", "{mode}");
        // Every delivered end of the refused callback was closed and physically ended.
        fixture.wait(|fixture| {
            fixture.native.closes.get() == closes + 16 && fixture.native.ended.get() == 32
        });
        fixture.settled();
        fixture.close();
    });
}

/// A callback ending while its stream import is still pending cancels that call and settles
/// only after its physical end; the late head is closed exactly once.
#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE"]
fn an_abandoned_pending_import_is_cancelled_and_joined() {
    engines(|interpreter, interpreted| {
        let mut fixture = install(interpreter, interpreted, limits(30_000), Kind::Native, None);
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
        fixture.wait(|fixture| {
            fixture
                .native
                .log
                .borrow()
                .iter()
                .any(|line| line == "gated:signalled")
        });
        // Logically cancelled, physically held: the consumer's call has not settled.
        for _ in 0..50 {
            fixture.pool.run_until_stalled();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(
            answer.borrow().is_none(),
            "{mode}: settled before the import's physical end"
        );
        assert_eq!(fixture.native.closes.get(), 0, "{mode}");
        assert_eq!(fixture.calls(), 2, "{mode}");
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
        fixture.close();
    });
}

/// A producer calling back into its busy consumer is refused at once, not at a timeout.
#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE"]
fn a_producer_calling_back_into_its_consumer_is_refused() {
    engines(|interpreter, interpreted| {
        let mut fixture = install(interpreter, interpreted, limits(5_000), Kind::Native, None);
        let mode = fixture.mode();
        *fixture.native.reenter.borrow_mut() = Some(fixture.reader.clone());
        let started = Instant::now();
        let (bytes, outcome) = fixture
            .call::<_, (u64, String)>("failure", (1_u32, 1_u32, REENTER))
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(2), "{mode}");
        assert_eq!(bytes, 0, "{mode}");
        assert!(
            outcome.starts_with("import:") && outcome.contains("reentr"),
            "{mode}: {outcome}"
        );
        fixture.native.reenter.borrow_mut().take();
        fixture.settled();
        fixture.close();
    });
}

/// Stream imports stay outside the data profile (platform ports), and every other stream shape
/// stays refused for a source importer on the native facade too.
#[test]
#[ignore = "requires TEMBRA_WASM_ENGINE"]
fn refused_import_profiles() {
    let Some(interpreter) = interpreter() else {
        return;
    };
    let (engine, _) = engine(&interpreter, false, limits(30_000));
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
    let artifact = tembra_contracts::Artifact::decode(
        &contract("consumer-orb")
            .source_artifact(&consumer_source(1), "reader", 1)
            .unwrap(),
    )
    .unwrap();
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
        "frames: func() -> stream<u8>;",
        "frames: async func(input: stream<u8>) -> u32;",
        "frames: async func() -> list<stream<u8>>;",
        "frames: async func() -> option<stream<u8>>;",
        "frames: async func() -> stream<u32>;",
        "frames: async func() -> future<u8>;",
        "resource session { frames: async func() -> stream<u8>; }",
    ] {
        let wit = format!(
            "package tembra-test:shapes@1.0.0; interface frames {{ {shape} }} \
             interface probe {{ ping: func() -> u32; }} \
             world orb {{ import frames; export probe; }}"
        );
        let artifact = Contract::from_source("shapes.wit", &wit, Some("orb"))
            .unwrap()
            .source_artifact(
                "globalThis.__tembraOrb = { services: { probe: { methods: { ping() { return 1; } } } } };",
                "orb",
                1,
            )
            .unwrap();
        let error = engine
            .load(
                &artifact,
                Install::default().requires("tembra-test:shapes/frames@1.0.0", "frames", 1),
            )
            .err()
            .unwrap_or_else(|| panic!("{shape} was admitted"));
        assert!(!error.message.is_empty(), "{shape}");
    }
    assert_eq!(engine.instances(), 0);
}
