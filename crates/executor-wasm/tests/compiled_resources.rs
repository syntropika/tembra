//! A compiled Orb's root resources and their dependencies: the component lists each resource
//! after the resources it depends on, the kernel opens them in that order and closes them in
//! reverse, and a dependency the component did not declare before it refuses the Orb.
//!
//! The fixture is a real component encoded from a small core module, so the test runs without a
//! guest toolchain. Its callbacks log each open at level info and each close at level warn, with
//! the resource's name as the message.
use futures::executor::LocalPool;
use std::sync::{Arc, Mutex};
use tembra::Host;
use tembra_contracts::ComponentRoles;
use tembra_executor_wasm::{Engine, Install, Limits, LogRecord};

#[path = "support/components.rs"]
mod components;

const WIT: &str = r#"
package example:stack@1.0.0;
interface probe {
  ping: func() -> u32;
}
world stack {
  import tembra:guest/context@1.0.0;
  import tembra:guest/resources@1.0.0;
  export tembra:guest/resources-init@1.0.0;
  export probe;
}
"#;

const PROBE: &str = "stack::example:stack/probe@1.0.0";

/// The module with `$declarations`, the data segments that list the resources: the names `a`,
/// `b` and `c` at 64, 65 and 66, dependency lists from 96, three declarations of 20 bytes
/// (name, version, dependencies) from 256, and the list itself at the return area 128.
fn module(declarations: &str) -> String {
    format!(
        r#"(module
  (import "tembra:guest/context@1.0.0" "log" (func $log (param i32 i32 i32)))
  (memory (export "memory") 1)
  (global $heap (mut i32) (i32.const 1024))
  (data (i32.const 64) "abc")
  {declarations}
  (func (export "cabi_realloc") (param $old i32) (param $old_size i32) (param $align i32) (param $size i32) (result i32)
    (local $block i32)
    global.get $heap
    local.get $align i32.const 1 i32.sub i32.add
    i32.const 0 local.get $align i32.sub i32.and
    local.tee $block
    local.get $size i32.add
    global.set $heap
    local.get $block)
  (func (export "tembra:guest/resources-init@1.0.0#declare") (result i32)
    i32.const 128)
  ;; open(name) -> u32: log the name at level info (2); the handle is 7.
  (func (export "tembra:guest/resources-init@1.0.0#open") (param i32 i32) (result i32)
    i32.const 2 local.get 0 local.get 1 call $log
    i32.const 7)
  ;; close(name, handle): log the name at level warn (3).
  (func (export "tembra:guest/resources-init@1.0.0#close") (param i32 i32 i32)
    i32.const 3 local.get 0 local.get 1 call $log)
  (func (export "example:stack/probe@1.0.0#ping") (result i32)
    i32.const 1))"#
    )
}

/// One declaration of 20 bytes at `at`: the one-byte name at `name`, version 1, and `count`
/// dependency names at `dependencies`.
fn declaration(at: u32, name: u32, dependencies: u32, count: u32) -> String {
    let bytes: String = [name, 1, 1, dependencies, count]
        .iter()
        .flat_map(|word| word.to_le_bytes())
        .map(|byte| format!("\\{byte:02x}"))
        .collect();
    format!(r#"(data (i32.const {at}) "{bytes}")"#)
}

/// The list of three declarations at 256, at the return area 128.
fn list() -> String {
    r#"(data (i32.const 128) "\00\01\00\00\03\00\00\00")"#.to_owned()
}

/// The dependency list `[name]` at `at`: the one-byte name at `name`.
fn names(at: u32, name: u32) -> String {
    let bytes: String = [name, 1]
        .iter()
        .flat_map(|word| word.to_le_bytes())
        .map(|byte| format!("\\{byte:02x}"))
        .collect();
    format!(r#"(data (i32.const {at}) "{bytes}")"#)
}

fn artifact(declarations: &str) -> Vec<u8> {
    components::encode(
        WIT,
        "stack",
        &module(declarations),
        "stack",
        &ComponentRoles::default(),
    )
}

fn engine() -> (Engine, Arc<Mutex<Vec<LogRecord>>>) {
    let records: Arc<Mutex<Vec<LogRecord>>> = Arc::default();
    let sink = records.clone();
    let engine = Engine::new(Limits::default())
        .unwrap()
        .on_log(move |record| sink.lock().unwrap().push(record.clone()));
    (engine, records)
}

/// `c` depends on `b`, `b` on `a`: the kernel opens `a`, `b`, `c` and closes `c`, `b`, `a`.
#[test]
fn root_resources_open_after_their_dependencies_and_close_before_them() {
    let declarations = [
        list(),
        names(96, 64),
        names(104, 65),
        declaration(256, 64, 96, 0),
        declaration(276, 65, 96, 1),
        declaration(296, 66, 104, 1),
    ]
    .join("\n  ");
    let (engine, records) = engine();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        engine
            .load(&artifact(&declarations), Install::default())
            .unwrap(),
    ])
    .unwrap();
    let probe = host.get(PROBE, vec!["main".into()], "alice").unwrap();
    pool.run_until(probe.ready()).unwrap();
    probe.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert!(host.errors().is_empty(), "{:?}", host.errors());
    let records: Vec<(String, String)> = records
        .lock()
        .unwrap()
        .iter()
        .map(|record| (record.level.clone(), record.message.clone()))
        .collect();
    let expected = [
        ("info", "a"),
        ("info", "b"),
        ("info", "c"),
        ("warn", "c"),
        ("warn", "b"),
        ("warn", "a"),
    ]
    .map(|(level, name)| (level.to_owned(), name.to_owned()));
    assert_eq!(records, expected);
}

/// `b` depends on `c`, which the component lists after it: the Orb is refused, naming both.
#[test]
fn a_dependency_not_declared_before_its_resource_refuses_the_orb() {
    let declarations = [
        list(),
        names(96, 66),
        declaration(256, 64, 96, 0),
        declaration(276, 65, 96, 1),
        declaration(296, 66, 96, 0),
    ]
    .join("\n  ");
    let (engine, records) = engine();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let installed = host
        .install(vec![
            engine
                .load(&artifact(&declarations), Install::default())
                .unwrap(),
        ])
        .and_then(|()| {
            let probe = host.get(PROBE, vec!["main".into()], "alice")?;
            pool.run_until(probe.ready())
        });
    let refused = installed.expect_err("the Orb is refused").to_string();
    assert!(
        refused.contains("the resource `b` depends on `c`"),
        "{refused}"
    );
    pool.run_until(host.close()).unwrap();
    assert!(
        records
            .lock()
            .unwrap()
            .iter()
            .all(|record| record.level != "info"),
        "no resource opens"
    );
}
