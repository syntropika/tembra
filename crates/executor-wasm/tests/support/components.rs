//! Real compiled components encoded from small core modules and one WIT package.
//!
//! Shared by executor and portable mobile transport tests, so they run without guest
//! toolchains. Interface identities are versioned `example:deps/*@1.0.0`.
#![allow(dead_code)]

pub const WIT: &str = r#"
package example:deps@1.0.0;

interface math { double: func(value: u32) -> u32; }
interface slow { delayed: async func(value: u32) -> u32; }
interface calc { quadruple: func(value: u32) -> u32; }
interface tail { run: async func(value: u32) -> u32; }

world math-provider { export math; }
world slow-provider { export slow; }
world consumer { import math; export calc; }
world tail-consumer { import slow; export tail; }
world eager-consumer { import math; export calc; }
"#;

pub const MATH: &str = "example:deps/math@1.0.0";
pub const SLOW: &str = "example:deps/slow@1.0.0";
pub const CALC: &str = "example:deps/calc@1.0.0";
pub const TAIL: &str = "example:deps/tail@1.0.0";

/// Doubles inside the provider component.
pub const MATH_PROVIDER: &str = r#"(module
  (func (export "example:deps/math@1.0.0#double") (param i32) (result i32)
    local.get 0 i32.const 2 i32.mul))"#;

/// Calls the synchronous `double` import twice.
pub const CONSUMER: &str = r#"(module
  (import "example:deps/math@1.0.0" "double" (func $double (param i32) (result i32)))
  (func (export "example:deps/calc@1.0.0#quadruple") (param i32) (result i32)
    local.get 0 call $double call $double))"#;

/// Returns its argument with `task.return` and yields. Only a later callback turn, after the
/// result has been published, issues the async import and waits for it. Globals hold the
/// argument, the turn and the waitable set; memory offset 16 receives the import result.
pub const TAIL_CONSUMER: &str = r#"(module
  (import "[export]example:deps/tail@1.0.0" "[task-return]run" (func $return (param i32)))
  (import "example:deps/slow@1.0.0" "[async-lower]delayed" (func $delayed (param i32 i32) (result i32)))
  (import "$root" "[waitable-set-new]" (func $set_new (result i32)))
  (import "$root" "[waitable-set-drop]" (func $set_drop (param i32)))
  (import "$root" "[waitable-join]" (func $join (param i32 i32)))
  (import "$root" "[subtask-drop]" (func $subtask_drop (param i32)))
  (memory (export "memory") 1)
  (global $value (mut i32) (i32.const 0))
  (global $turn (mut i32) (i32.const 0))
  (global $set (mut i32) (i32.const 0))
  (func (export "cabi_realloc") (param i32 i32 i32 i32) (result i32) i32.const 64)
  (func $wait (result i32)
    global.get $set i32.const 4 i32.shl i32.const 2 i32.or)
  (func (export "[async-lift]example:deps/tail@1.0.0#run") (param $value i32) (result i32)
    local.get $value
    global.set $value
    i32.const 0
    global.set $turn
    local.get $value
    call $return
    ;; YIELD: the published result is observable before the import below exists.
    i32.const 1)
  (func (export "[callback][async-lift]example:deps/tail@1.0.0#run")
    (param $event i32) (param $handle i32) (param $status i32) (result i32)
    (local $called i32)
    global.get $turn
    i32.eqz
    if (result i32)
      i32.const 1
      global.set $turn
      global.get $value
      i32.const 16
      call $delayed
      local.tee $called
      i32.const 15
      i32.and
      i32.const 2
      i32.eq
      if (result i32)
        i32.const 0
      else
        call $set_new
        global.set $set
        local.get $called
        i32.const 4
        i32.shr_u
        global.get $set
        call $join
        call $wait
      end
    else
      local.get $event
      i32.const 1
      i32.eq
      local.get $status
      i32.const 2
      i32.eq
      i32.and
      if (result i32)
        local.get $handle
        i32.const 0
        call $join
        local.get $handle
        call $subtask_drop
        global.get $set
        call $set_drop
        i32.const 0
      else
        call $wait
      end
    end))"#;

/// Calls its dependency from a core start function, before any invocation exists.
pub const EAGER_CONSUMER: &str = r#"(module
  (import "example:deps/math@1.0.0" "double" (func $double (param i32) (result i32)))
  (func $init i32.const 1 call $double drop)
  (start $init)
  (func (export "example:deps/calc@1.0.0#quadruple") (param i32) (result i32)
    local.get 0 call $double call $double))"#;

/// Encode a core module and its WIT world as a component, then package it as an Orb artifact.
pub fn artifact(world: &str, module: &str, name: &str) -> Vec<u8> {
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve.push_str("deps.wit", WIT).unwrap();
    let selected = resolve.select_world(&[package], Some(world)).unwrap();
    let mut core = wat::parse_str(module).unwrap();
    wit_component::embed_component_metadata(
        &mut core,
        &resolve,
        selected,
        wit_component::StringEncoding::UTF8,
        false,
    )
    .unwrap();
    let component = wit_component::ComponentEncoder::default()
        .validate(true)
        .module(&core)
        .unwrap()
        .encode()
        .unwrap();
    tembra_contracts::Contract::from_source("deps.wit", WIT, Some(world))
        .unwrap()
        .component_artifact(&component, name, 1, &Default::default())
        .unwrap()
}
