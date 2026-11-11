//! Runtime edges found by SDK tests, built through the production preparer against the real
//! runtime named by `TEMBRA_QJS_RUNTIME` and run in Wasmtime:
//!
//! - an invalid `stream` argument is refused with a catchable `TypeError` before the import is
//!   called, so an owned resource passed earlier in the same call stays with the guest;
//! - the standard `Date` constructor reached through reflection behaves like `Date` itself and
//!   reads the host's runtime-internal clock.
//!
//! Every probe runs on a fresh instance, so one trap cannot hide another result.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use wasmtime::component::{
    Component, Instance, Linker, Resource, ResourceTable, ResourceType, Source, StreamConsumer,
    StreamReader, StreamResult, Val,
};
use wasmtime::{Config, Engine, Store, StoreContextMut};
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;
use wasmtime_wasi::{HostWallClock, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use super::componentize;

const WIT: &str = r#"
package tembra-probe:edges;

interface host {
  resource thing {
    alive: func() -> bool;
  }
  make: func() -> thing;
  take: func(owned: thing, body: option<stream<u8>>) -> u32;
}

interface probe {
  body: async func(name: string) -> string;
  clock: func(name: string) -> string;
}

world orb {
  import host;
  export probe;
}
"#;

const JS: &str = r#"
import host from "tembra-probe:edges/host";
const outcome = (error) => `${error?.constructor?.name}:${error?.message}`;
const bodies = {
  none: () => undefined,
  bytes: () => new Uint8Array([1, 2, 3]),
  array: () => [4, 5],
  generator: () => (function* () { yield new Uint8Array([6, 7]); })(),
  async: () => (async function* () { yield new Uint8Array([8]); })(),
  readable: () => {
    const { readable, writable } = wit.Stream(wit.Stream.U8);
    writable.write(new Uint8Array([9, 9])).then(() => writable.drop());
    return readable;
  },
  object: () => ({}),
  string: () => "text",
  fraction: () => 1.5,
  transferred: () => {
    const { readable, writable } = wit.Stream(wit.Stream.U8);
    writable.drop();
    host.take(host.make(), readable);
    return readable;
  },
};
const clocks = {
  now: () => Date.now(),
  construct: () => new Date().getTime(),
  identity: () => new Date(0).constructor === Date && Object.getPrototypeOf(new Date(0)) === Date.prototype,
  reflective: () => new (new Date(0).constructor)().getTime(),
  reflect: () => Reflect.construct(Date, []).getTime(),
  call: () => typeof Date(),
  parse: () => Date.parse("2020-01-01T00:00:00Z") + Date.UTC(2020, 0, 2),
  arithmetic: () => new Date(86400000).getUTCDate(),
  emitted: () => { try { return new new Date(0).constructor().getTime(); } catch (e) { return outcome(e); } },
  emittedMap: () => { try { return new new Map([[1, 2]]).constructor().size; } catch (e) { return outcome(e); } },
  receiver: () => { try { Date.prototype.getTime.call({}); return "accepted"; } catch (e) { return outcome(e); } },
};
export const probe = {
  async body(name) {
    const owned = host.make();
    try {
      const taken = host.take(owned, bodies[name]());
      return `ok:${taken}`;
    } catch (error) {
      return `${outcome(error)}|owned:${owned.alive()}`;
    }
  },
  clock(name) { return String(clocks[name]()); },
};
"#;

fn runtime() -> Vec<u8> {
    let path = std::env::var_os("TEMBRA_QJS_RUNTIME")
        .expect("set TEMBRA_QJS_RUNTIME to the prebuilt patched runtime dylib");
    std::fs::read(path).expect("read TEMBRA_QJS_RUNTIME")
}

fn component() -> &'static [u8] {
    static BYTES: OnceLock<Vec<u8>> = OnceLock::new();
    BYTES.get_or_init(|| {
        let world = tembra_contracts::Contract::from_source("orb.wit", WIT, Some("orb")).unwrap();
        componentize(&world, JS, &runtime()).unwrap()
    })
}

struct Thing;

struct Host {
    wasi: WasiCtx,
    table: ResourceTable,
    /// Bytes the host read from every `take` body.
    received: Arc<AtomicUsize>,
}

impl WasiView for Host {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

const CLOCK_MS: u64 = 1_700_000_000_000;

/// The host's runtime-internal wall clock: configured (clocks grant) or frozen at zero (none).
struct Wall(u64);

impl HostWallClock for Wall {
    fn resolution(&self) -> Duration {
        Duration::from_nanos(1)
    }
    fn now(&self) -> Duration {
        Duration::from_millis(self.0)
    }
}

/// Counts body bytes until the writer ends the stream.
struct Count(Arc<AtomicUsize>);

impl StreamConsumer<Host> for Count {
    type Item = u8;

    fn poll_consume(
        self: std::pin::Pin<&mut Self>,
        _: &mut Context<'_>,
        mut store: StoreContextMut<'_, Host>,
        mut source: Source<'_, u8>,
        _finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        let mut bytes = Vec::with_capacity(source.remaining(&mut store)?);
        source.read(&mut store, &mut bytes)?;
        self.0.fetch_add(bytes.len(), Ordering::SeqCst);
        Poll::Ready(Ok(StreamResult::Completed))
    }
}

fn block<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
        .block_on(future)
}

async fn instantiate(clock_ms: u64) -> (Store<Host>, Instance, MemoryOutputPipe) {
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);
    let engine = Engine::new(&config).unwrap();
    let component = Component::new(&engine, component()).unwrap();
    let mut linker = Linker::<Host>::new(&engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker).unwrap();
    let mut host = linker.instance("tembra-probe:edges/host").unwrap();
    host.resource("thing", ResourceType::host::<Thing>(), |mut store, rep| {
        store
            .data_mut()
            .table
            .delete(Resource::<Thing>::new_own(rep))?;
        Ok(())
    })
    .unwrap();
    host.func_wrap(
        "[method]thing.alive",
        |mut store, (thing,): (Resource<Thing>,)| Ok((store.data_mut().table.get(&thing).is_ok(),)),
    )
    .unwrap();
    host.func_wrap("make", |mut store, ()| {
        Ok((store.data_mut().table.push(Thing)?,))
    })
    .unwrap();
    host.func_wrap(
        "take",
        |mut store, (owned, body): (Resource<Thing>, Option<StreamReader<u8>>)| {
            store.data_mut().table.delete(owned)?;
            let received = store.data().received.clone();
            if let Some(body) = body {
                body.pipe(&mut store, Count(received))?;
            }
            Ok((1u32,))
        },
    )
    .unwrap();
    let stderr = MemoryOutputPipe::new(64 * 1024);
    let wasi = WasiCtxBuilder::new()
        .stderr(stderr.clone())
        .wall_clock(Wall(clock_ms))
        .build();
    let host = Host {
        wasi,
        table: ResourceTable::new(),
        received: Arc::default(),
    };
    let mut store = Store::new(&engine, host);
    let instance = linker
        .instantiate_async(&mut store, &component)
        .await
        .unwrap();
    (store, instance, stderr)
}

/// Call one probe export on a fresh instance; a trap is reported with the guest's stderr.
fn probe(export: &str, name: &str) -> (Result<String, String>, usize) {
    probe_at(export, name, CLOCK_MS)
}

fn probe_at(export: &str, name: &str, clock_ms: u64) -> (Result<String, String>, usize) {
    block(async {
        let (mut store, instance, stderr) = instantiate(clock_ms).await;
        let interface = instance
            .get_export_index(&mut store, None, "tembra-probe:edges/probe")
            .unwrap();
        let index = instance
            .get_export_index(&mut store, Some(&interface), export)
            .unwrap();
        let func = instance.get_func(&mut store, index).unwrap();
        let mut results = [Val::Bool(false)];
        let outcome = func
            .call_async(&mut store, &[Val::String(name.into())], &mut results)
            .await;
        // Let the host drain any body stream the guest is still writing.
        let _ = store
            .run_concurrent(async |_| tokio::time::sleep(Duration::from_millis(20)).await)
            .await;
        let received = store.data().received.load(Ordering::SeqCst);
        let result = match (outcome, &results[0]) {
            (Ok(()), Val::String(text)) => Ok(text.clone()),
            (Ok(()), other) => Ok(format!("{other:?}")),
            (Err(error), _) => Err(format!(
                "trap: {}; stderr: {}",
                error.root_cause(),
                String::from_utf8_lossy(&stderr.contents())
            )),
        };
        (result, received)
    })
}

#[test]
fn valid_stream_arguments_reach_the_host() {
    for (name, bytes) in [
        ("none", 0),
        ("bytes", 3),
        ("array", 2),
        ("generator", 2),
        ("async", 1),
        ("readable", 2),
    ] {
        let (result, received) = probe("body", name);
        assert_eq!(result.as_deref(), Ok("ok:1"), "{name}");
        assert_eq!(received, bytes, "{name}");
    }
}

#[test]
fn an_invalid_stream_is_refused_before_an_owned_argument_moves() {
    for name in ["object", "string", "fraction", "transferred"] {
        let (result, received) = probe("body", name);
        let text = result.unwrap_or_else(|trap| panic!("{name} trapped: {trap}"));
        assert!(text.starts_with("TypeError:"), "{name}: {text}");
        assert!(text.ends_with("|owned:true"), "{name}: {text}");
        assert_eq!(received, 0, "{name}");
    }
}

#[test]
fn the_date_constructor_is_the_same_through_reflection() {
    let expect = [
        ("now", CLOCK_MS.to_string()),
        ("construct", CLOCK_MS.to_string()),
        ("identity", "true".into()),
        ("reflective", CLOCK_MS.to_string()),
        ("reflect", CLOCK_MS.to_string()),
        ("call", "string".into()),
        (
            "parse",
            (1_577_836_800_000u64 + 1_577_923_200_000).to_string(),
        ),
        ("arithmetic", "2".into()),
        ("emitted", CLOCK_MS.to_string()),
        ("emittedMap", "0".into()),
        ("receiver", "TypeError:not a Date object".into()),
    ];
    for (name, want) in expect {
        let (result, _) = probe("clock", name);
        assert_eq!(result, Ok(want), "{name}");
    }
}

/// Without the clocks grant the host's internal clock is frozen at zero.
#[test]
fn the_date_constructor_reads_a_frozen_clock_through_reflection() {
    for (name, want) in [
        ("now", "0"),
        ("construct", "0"),
        ("reflective", "0"),
        ("reflect", "0"),
        ("call", "string"),
        ("arithmetic", "2"),
    ] {
        let (result, _) = probe_at("clock", name, 0);
        assert_eq!(result.as_deref(), Ok(want), "{name}");
    }
}
