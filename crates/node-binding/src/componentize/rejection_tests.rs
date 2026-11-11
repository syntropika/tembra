//! Recoverable WIT `result` errors from asynchronous imports stay catchable in JavaScript.
//!
//! Built through the production preparer against the real runtime named by
//! `TEMBRA_QJS_RUNTIME`, then run in Wasmtime with a host that rejects each import.

use std::sync::OnceLock;

use wasmtime::component::{
    Accessor, Component, ComponentType, Instance, Lift, Linker, Lower, Resource, ResourceTable,
    ResourceType,
};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use super::componentize;

const WIT: &str = r#"
package tembra-probe:reject;

interface host {
  resource thing {
    name: func() -> string;
    reopen: async func() -> result<thing, code>;
    slow: async func() -> result<string, code>;
  }
  make: func() -> thing;
  variant code {
    denied,
    other(option<string>),
  }
  fail-text: async func() -> result<string, string>;
  fail-thing: async func() -> result<thing, string>;
  fail-code: async func() -> result<thing, code>;
  sync-fail-thing: func() -> result<thing, code>;
}

interface probe {
  run: async func(name: string) -> string;
  dispose-while-borrowed: async func() -> result<string, string>;
}

world orb {
  import host;
  export probe;
}
"#;

const JS: &str = r#"
import host from "tembra-probe:reject/host";
const describe = (error) =>
  `${error?.constructor?.name}:${JSON.stringify(error?.payload)}`;
export const probe = {
  async disposeWhileBorrowed() {
    const thing = host.make();
    const pending = thing.slow().then(
      () => "resolved",
      (error) => `slow:${describe(error)}`,
    );
    let early;
    try {
      thing[Symbol.dispose]();
      early = "disposed";
    } catch (error) {
      early = `refused:${error.message}`;
    }
    const settled = await pending;
    thing[Symbol.dispose]();
    return `${early}|${settled}|disposed`;
  },
  async run(name) {
    try {
      if (name === "reopen") {
        using thing = host.make();
        await thing.reopen();
      } else await host[name]();
      return "resolved";
    } catch (error) {
      return `caught:${describe(error)}`;
    }
  },
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

#[derive(ComponentType, Lift, Lower, Clone, Debug)]
#[component(variant)]
enum Code {
    #[component(name = "denied")]
    Denied,
    #[component(name = "other")]
    Other(Option<String>),
}

struct Host {
    wasi: WasiCtx,
    table: ResourceTable,
}

impl WasiView for Host {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

async fn instantiate() -> (Store<Host>, Instance) {
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);
    let engine = Engine::new(&config).unwrap();
    let component = Component::new(&engine, component()).unwrap();
    let mut linker = Linker::<Host>::new(&engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker).unwrap();
    let mut host = linker.instance("tembra-probe:reject/host").unwrap();
    host.resource("thing", ResourceType::host::<Thing>(), |_, _| Ok(()))
        .unwrap();
    host.func_wrap("[method]thing.name", |_, (_,): (Resource<Thing>,)| {
        Ok(("thing".to_owned(),))
    })
    .unwrap();
    host.func_wrap("make", |mut store, ()| {
        Ok((store.data_mut().table.push(Thing)?,))
    })
    .unwrap();
    host.func_wrap_concurrent(
        "[method]thing.reopen",
        |_: &Accessor<Host>, (_,): (Resource<Thing>,)| {
            Box::pin(async { Ok((Err::<Resource<Thing>, Code>(Code::Other(None)),)) })
        },
    )
    .unwrap();
    host.func_wrap_concurrent(
        "[method]thing.slow",
        |_: &Accessor<Host>, (_,): (Resource<Thing>,)| {
            Box::pin(async {
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                Ok((Err::<String, Code>(Code::Denied),))
            })
        },
    )
    .unwrap();
    host.func_wrap_concurrent("fail-text", |_: &Accessor<Host>, ()| {
        Box::pin(async { Ok((Err::<String, String>("text refused".into()),)) })
    })
    .unwrap();
    host.func_wrap_concurrent("fail-thing", |_: &Accessor<Host>, ()| {
        Box::pin(async { Ok((Err::<Resource<Thing>, String>("thing refused".into()),)) })
    })
    .unwrap();
    host.func_wrap_concurrent("fail-code", |_: &Accessor<Host>, ()| {
        Box::pin(async { Ok((Err::<Resource<Thing>, Code>(Code::Denied),)) })
    })
    .unwrap();
    host.func_wrap("sync-fail-thing", |_, ()| {
        Ok((Err::<Resource<Thing>, Code>(Code::Other(Some(
            "sync".into(),
        ))),))
    })
    .unwrap();
    let wasi = WasiCtxBuilder::new().build();
    let mut store = Store::new(
        &engine,
        Host {
            wasi,
            table: ResourceTable::new(),
        },
    );
    let instance = linker
        .instantiate_async(&mut store, &component)
        .await
        .unwrap();
    (store, instance)
}

async fn run(store: &mut Store<Host>, instance: &Instance, name: &str) -> wasmtime::Result<String> {
    let interface = instance
        .get_export_index(&mut *store, None, "tembra-probe:reject/probe")
        .unwrap();
    let index = instance
        .get_export_index(&mut *store, Some(&interface), "run")
        .unwrap();
    let func = instance
        .get_typed_func::<(String,), (String,)>(&mut *store, index)
        .unwrap();
    Ok(func.call_async(&mut *store, (name.to_owned(),)).await?.0)
}

fn block<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

#[test]
fn rejected_async_import_results_are_catchable() {
    block(async {
        let mut outcomes = Vec::new();
        for name in [
            "failText",
            "failThing",
            "failCode",
            "syncFailThing",
            "reopen",
        ] {
            let (mut store, instance) = instantiate().await;
            let outcome = run(&mut store, &instance, name).await.map_err(|error| {
                format!("{error:?}")
                    .lines()
                    .take(18)
                    .collect::<Vec<_>>()
                    .join("\n")
            });
            outcomes.push((name, outcome));
        }
        for (name, outcome) in &outcomes {
            eprintln!("PROBE {name}: {outcome:?}");
        }
        let expected = [
            ("failText", "caught:Error:\"text refused\""),
            ("failThing", "caught:Error:\"thing refused\""),
            ("failCode", "caught:Error:{\"tag\":\"denied\"}"),
            (
                "syncFailThing",
                "caught:Error:{\"tag\":\"other\",\"val\":\"sync\"}",
            ),
            ("reopen", "caught:Error:{\"tag\":\"other\"}"),
        ];
        for ((name, outcome), (_, value)) in outcomes.iter().zip(expected) {
            assert_eq!(outcome.as_deref(), Ok(value), "{name}");
        }
    });
}

const FILES_WIT: &str = r#"
package tembra-probe:files;

interface files {
  escape: async func(disposed: bool) -> string;
}

world orb {
  import wasi:filesystem/types@0.3.0;
  import wasi:filesystem/preopens@0.3.0;
  export files;
}
"#;

const FILES_JS: &str = r#"
import { getDirectories } from "wasi:filesystem/preopens@0.3.0";
const describe = (error) =>
  `${error?.constructor?.name}:${JSON.stringify(error?.payload)}`;
export const files = {
  async escape(disposed) {
    const [[directory]] = getDirectories();
    try {
      if (disposed) {
        using _file = await directory.openAt({}, "../secret", {}, { read: true });
      } else await directory.openAt({}, "../secret", {}, { read: true });
      return "opened";
    } catch (error) {
      return `caught:${describe(error)}`;
    } finally {
      directory[Symbol.dispose]();
    }
  },
};
"#;

/// While a pending import borrows a resource, disposing it is refused recoverably; once the
/// import settles (here with a WIT `err`), disposal succeeds. Disposing early must never be
/// silently accepted, and the refusal must not trap the instance.
#[test]
fn disposing_a_resource_borrowed_by_a_pending_import_is_refused_recoverably() {
    block(async {
        let (mut store, instance) = instantiate().await;
        let interface = instance
            .get_export_index(&mut store, None, "tembra-probe:reject/probe")
            .unwrap();
        let index = instance
            .get_export_index(&mut store, Some(&interface), "dispose-while-borrowed")
            .unwrap();
        let func = instance
            .get_typed_func::<(), (Result<String, String>,)>(&mut store, index)
            .unwrap();
        let (outcome,) = func.call_async(&mut store, ()).await.unwrap();
        assert_eq!(
            outcome,
            Ok("refused:cannot dispose a resource while it is borrowed|slow:Error:{\"tag\":\"denied\"}|disposed".into())
        );
        // The instance keeps serving after the refusal.
        assert_eq!(
            run(&mut store, &instance, "failText").await.unwrap(),
            "caught:Error:\"text refused\""
        );
    });
}

/// A standard WASI 0.3 path escape is a recoverable `error-code`, not a trap.
#[test]
fn wasi_filesystem_path_escape_is_a_catchable_error_code() {
    let world =
        tembra_contracts::Contract::from_source("files.wit", FILES_WIT, Some("orb")).unwrap();
    let bytes = componentize(&world, FILES_JS, &runtime()).unwrap();
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("input"), "x").unwrap();
    block(async {
        let mut outcomes = Vec::new();
        for disposed in [false, true] {
            let mut config = Config::new();
            config.wasm_component_model(true);
            config.wasm_component_model_async(true);
            let engine = Engine::new(&config).unwrap();
            let component = Component::new(&engine, &bytes).unwrap();
            let mut linker = Linker::<Host>::new(&engine);
            wasmtime_wasi::p2::add_to_linker_async(&mut linker).unwrap();
            wasmtime_wasi::p3::add_to_linker(&mut linker).unwrap();
            let mut builder = WasiCtxBuilder::new();
            builder
                .preopened_dir(directory.path(), "/data", wasmtime_wasi::FsPerms::ReadWrite)
                .unwrap();
            let wasi = builder.build();
            let mut store = Store::new(
                &engine,
                Host {
                    wasi,
                    table: ResourceTable::new(),
                },
            );
            let instance = linker
                .instantiate_async(&mut store, &component)
                .await
                .unwrap();
            let interface = instance
                .get_export_index(&mut store, None, "tembra-probe:files/files")
                .unwrap();
            let index = instance
                .get_export_index(&mut store, Some(&interface), "escape")
                .unwrap();
            let func = instance
                .get_typed_func::<(bool,), (String,)>(&mut store, index)
                .unwrap();
            let outcome = func
                .call_async(&mut store, (disposed,))
                .await
                .map(|(value,)| value)
                .map_err(|error| {
                    format!("{error:?}")
                        .lines()
                        .take(18)
                        .collect::<Vec<_>>()
                        .join("\n")
                });
            eprintln!("PROBE escape disposed={disposed}: {outcome:?}");
            outcomes.push(outcome);
        }
        for outcome in outcomes {
            assert_eq!(
                outcome.as_deref(),
                Ok("caught:Error:{\"tag\":\"not-permitted\"}")
            );
        }
    });
}
