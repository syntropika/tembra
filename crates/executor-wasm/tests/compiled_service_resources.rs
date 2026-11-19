//! A compiled Orb's service resources: one of each per object of the service, declared when the
//! kernel creates the object, opened after the root's resources they depend on and closed when
//! the object goes, before the root's. A call reads the resource of the object it runs on, else
//! the root's.
//!
//! The fixture is a real component encoded from a small core module, so the test runs without a
//! guest toolchain. Its root owns `a`; each object of its service owns `x`, which depends on `a`,
//! and `y`, which depends on `x`. Its callbacks log each open at level info and each close at
//! level warn, with the resource's name as the message, and hand out handles in opening order;
//! when `down` says so, the `y` that would get the handle 5 answers `err("down")` instead.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use tembra::{Host, Reference};
use tembra_contracts::ComponentRoles;
use tembra_executor_wasm::{Engine, Install, Limits, LogRecord};

#[path = "support/components.rs"]
mod components;

/// The world, whose root owns resources when `root` says so.
fn wit(root: bool) -> String {
    let root = match root {
        true => "export tembra:guest/resources-init@1.0.0;",
        false => "",
    };
    format!(
        r#"
package example:sockets@1.0.0;
interface probe {{
  /// The handle of this object's `y`.
  mine: func() -> u32;
  /// The handle of the root's `a`.
  shared: func() -> u32;
}}
world sockets {{
  import tembra:guest/context@1.0.0;
  import tembra:guest/resources@1.0.0;
  {root}
  export tembra:guest/service-resources-init@1.0.0;
  export probe;
}}
"#
    )
}

const PROBE: &str = "sockets::example:sockets/probe@1.0.0";

/// The names `a`, `x` and `y` at 64, 65 and 66; the dependency lists `[a]` at 96 and `[x]` at
/// 104; the root's one declaration of 20 bytes at 256 and the service's two from 276; the root's
/// list at the return area 128 and the service's at 136; the message `down` at 72. Without a root
/// resource, `x` depends on nothing. The result of an open is written at 512.
fn module(root: bool, down: bool) -> String {
    let down = i32::from(down);
    let data = [
        r#"(data (i32.const 64) "axy") (data (i32.const 72) "down")"#.to_owned(),
        names(96, 64),
        names(104, 65),
        list(128, 256, 1),
        list(136, 276, 2),
        declaration(256, 64, 96, 0),
        declaration(276, 65, 96, u32::from(root)),
        declaration(296, 66, 104, 1),
    ]
    .join("\n  ");
    let root_callbacks = match root {
        true => {
            r#"
  (func (export "tembra:guest/resources-init@1.0.0#declare") (result i32)
    i32.const 128)
  (func (export "tembra:guest/resources-init@1.0.0#open") (param i32 i32) (result i32)
    local.get 0 local.get 1 call $open)
  (func (export "tembra:guest/resources-init@1.0.0#close") (param i32 i32 i32)
    local.get 0 local.get 1 call $close)"#
        }
        false => "",
    };
    format!(
        r#"(module
  (import "tembra:guest/context@1.0.0" "log" (func $log (param i32 i32 i32)))
  (import "tembra:guest/resources@1.0.0" "read" (func $read (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  (global $heap (mut i32) (i32.const 1024))
  ;; The last handle handed out: each open answers the next one.
  (global $last (mut i32) (i32.const 0))
  {data}
  (func (export "cabi_realloc") (param $old i32) (param $old_size i32) (param $align i32) (param $size i32) (result i32)
    (local $block i32)
    global.get $heap
    local.get $align i32.const 1 i32.sub i32.add
    i32.const 0 local.get $align i32.sub i32.and
    local.tee $block
    local.get $size i32.add
    global.set $heap
    local.get $block)
  ;; open -> result<u32, string>: the `y` that would be 5 is down when the module says so;
  ;; otherwise log the name at level info (2) and answer the next handle.
  (func $open (param $name i32) (param $len i32) (result i32)
    local.get $name i32.load8_u i32.const 121 i32.eq
    global.get $last i32.const 4 i32.eq i32.and
    i32.const {down} i32.and
    if
      (i32.store8 (i32.const 512) (i32.const 1))
      (i32.store (i32.const 516) (i32.const 72))
      (i32.store (i32.const 520) (i32.const 4))
    else
      i32.const 2 local.get $name local.get $len call $log
      global.get $last i32.const 1 i32.add
      global.set $last
      (i32.store8 (i32.const 512) (i32.const 0))
      (i32.store (i32.const 516) (global.get $last))
    end
    i32.const 512)
  ;; close: log the name at level warn (3).
  (func $close (param $name i32) (param $len i32)
    i32.const 3 local.get $name local.get $len call $log)
{root_callbacks}
  ;; declare(service) -> list<declaration>: the same two for the only service.
  (func (export "tembra:guest/service-resources-init@1.0.0#declare") (param i32 i32) (result i32)
    i32.const 136)
  (func (export "tembra:guest/service-resources-init@1.0.0#open") (param i32 i32 i32 i32) (result i32)
    local.get 2 local.get 3 call $open)
  (func (export "tembra:guest/service-resources-init@1.0.0#close") (param i32 i32 i32 i32 i32)
    local.get 2 local.get 3 call $close)
  (func (export "example:sockets/probe@1.0.0#mine") (result i32)
    i32.const 66 i32.const 1 call $read)
  (func (export "example:sockets/probe@1.0.0#shared") (result i32)
    i32.const 64 i32.const 1 call $read))"#
    )
}

/// One declaration of 20 bytes at `at`: the one-byte name at `name`, version 1, and `count`
/// dependency names at `dependencies`.
fn declaration(at: u32, name: u32, dependencies: u32, count: u32) -> String {
    words(at, &[name, 1, 1, dependencies, count])
}

/// The list of `count` declarations at `declarations`, at the return area `at`.
fn list(at: u32, declarations: u32, count: u32) -> String {
    words(at, &[declarations, count])
}

/// The dependency list `[name]` at `at`: the one-byte name at `name`.
fn names(at: u32, name: u32) -> String {
    words(at, &[name, 1])
}

fn words(at: u32, words: &[u32]) -> String {
    let bytes: String = words
        .iter()
        .flat_map(|word| word.to_le_bytes())
        .map(|byte| format!("\\{byte:02x}"))
        .collect();
    format!(r#"(data (i32.const {at}) "{bytes}")"#)
}

fn artifact(root: bool, down: bool) -> Vec<u8> {
    components::encode(
        &wit(root),
        "sockets",
        &module(root, down),
        "sockets",
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

/// The level and message of each record so far.
fn logged(records: &Mutex<Vec<LogRecord>>) -> Vec<(String, String)> {
    records
        .lock()
        .unwrap()
        .iter()
        .map(|record| (record.level.clone(), record.message.clone()))
        .collect()
}

fn expected(records: &[(&str, &str)]) -> Vec<(String, String)> {
    records
        .iter()
        .map(|(level, name)| (level.to_string(), name.to_string()))
        .collect()
}

/// Call one method under a root of `alice`.
fn call(pool: &mut LocalPool, host: &Host, reference: &Rc<Reference>, method: &'static str) -> u32 {
    let reference = reference.clone();
    let value: Value = pool
        .run_until(host.root("alice", move |context| {
            async move {
                reference
                    .call::<_, Value>(&context, method, json!([]))
                    .await
            }
            .boxed_local()
        }))
        .unwrap();
    serde_json::from_value(value).unwrap()
}

/// The object of `key`, ready.
fn object(pool: &mut LocalPool, host: &Host, key: &str) -> Rc<Reference> {
    let reference = Rc::new(host.get(PROBE, vec![key.into()], "alice").unwrap());
    pool.run_until(reference.ready()).unwrap();
    reference
}

/// Two objects open `x` and `y` of their own after the root's `a`, each reading its own `y` and
/// the root's `a`; releasing one closes its own `y` and `x` only; the other's close at teardown,
/// and the root's `a` last.
#[test]
fn each_object_opens_its_resources_after_the_roots_and_closes_them_before() {
    let (engine, records) = engine();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        engine
            .load(&artifact(true, false), Install::default())
            .unwrap(),
    ])
    .unwrap();
    let one = object(&mut pool, &host, "one");
    let two = object(&mut pool, &host, "two");
    assert_eq!(
        logged(&records),
        expected(&[
            ("info", "a"),
            ("info", "x"),
            ("info", "y"),
            ("info", "x"),
            ("info", "y"),
        ])
    );
    // The handles are the opening order: `a` is 1; `one` opened 2 and 3, `two` 4 and 5.
    assert_eq!(call(&mut pool, &host, &one, "mine"), 3);
    assert_eq!(call(&mut pool, &host, &two, "mine"), 5);
    assert_eq!(call(&mut pool, &host, &one, "shared"), 1);
    assert_eq!(call(&mut pool, &host, &two, "shared"), 1);
    one.release().unwrap();
    pool.run_until(host.settled());
    assert_eq!(
        logged(&records)[5..],
        expected(&[("warn", "y"), ("warn", "x")])
    );
    assert_eq!(call(&mut pool, &host, &two, "mine"), 5);
    two.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert!(host.errors().is_empty(), "{:?}", host.errors());
    assert_eq!(
        logged(&records)[7..],
        expected(&[("warn", "y"), ("warn", "x"), ("warn", "a")])
    );
}

/// A world whose services own resources and whose root owns none exports
/// `service-resources-init` alone: the object's `x` and `y` open and close.
#[test]
fn a_service_owns_resources_without_the_root() {
    let (engine, records) = engine();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        engine
            .load(&artifact(false, false), Install::default())
            .unwrap(),
    ])
    .unwrap();
    let one = object(&mut pool, &host, "one");
    assert_eq!(call(&mut pool, &host, &one, "mine"), 2);
    one.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert!(host.errors().is_empty(), "{:?}", host.errors());
    assert_eq!(
        logged(&records),
        expected(&[("info", "x"), ("info", "y"), ("warn", "y"), ("warn", "x")])
    );
}

/// The second object's `y` answers `err("down")` after its `x` opened: that object's creation
/// fails with the message and the kernel closes its `x`; the first object and the root's `a`
/// keep working, and close as before.
#[test]
fn an_object_whose_open_answers_an_error_is_not_created_and_closes_what_opened() {
    let (engine, records) = engine();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        engine
            .load(&artifact(true, true), Install::default())
            .unwrap(),
    ])
    .unwrap();
    let one = object(&mut pool, &host, "one");
    let two = Rc::new(host.get(PROBE, vec!["two".into()], "alice").unwrap());
    let refused = pool
        .run_until(two.ready())
        .expect_err("the object is not created");
    assert_eq!(
        (
            refused.code,
            refused.reason.as_str(),
            refused.message.as_str()
        ),
        (
            tembra::core::Code::Unavailable,
            "resource",
            "the resource `y` did not open: down"
        ),
        "{refused}"
    );
    assert_eq!(
        logged(&records),
        expected(&[
            ("info", "a"),
            ("info", "x"),
            ("info", "y"),
            ("info", "x"),
            ("warn", "x"),
        ])
    );
    assert_eq!(call(&mut pool, &host, &one, "mine"), 3);
    assert_eq!(call(&mut pool, &host, &one, "shared"), 1);
    one.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert!(host.errors().is_empty(), "{:?}", host.errors());
    assert_eq!(
        logged(&records)[5..],
        expected(&[("warn", "y"), ("warn", "x"), ("warn", "a")])
    );
}
