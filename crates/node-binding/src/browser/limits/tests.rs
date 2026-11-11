//! Instrumented modules executed by Wasmtime under the host contract the browser follows: one
//! shared `fuel` and `pages` pair per component instance, and `initial_pages` debited before
//! every instantiation of a module.
use wasmtime::{
    Config, Engine, Global, GlobalType, Instance, Linker, Module, Mutability, Store, Trap, Val,
    ValType,
};

use super::{LIMITS_MODULE, instrument};

fn engine() -> Engine {
    let mut config = Config::new();
    config
        .wasm_tail_call(true)
        .wasm_multi_memory(true)
        .wasm_memory64(true);
    Engine::new(&config).unwrap()
}

/// One component instance's shared limits.
struct Limits {
    store: Store<()>,
    fuel: Global,
    pages: Global,
}

impl Limits {
    fn new(engine: &Engine, fuel: i64, pages: i64) -> Self {
        let mut store = Store::new(engine, ());
        let ty = GlobalType::new(ValType::I64, Mutability::Var);
        let fuel = Global::new(&mut store, ty.clone(), Val::I64(fuel)).unwrap();
        let pages = Global::new(&mut store, ty, Val::I64(pages)).unwrap();
        Self { store, fuel, pages }
    }

    fn fuel(&mut self) -> i64 {
        self.fuel.get(&mut self.store).unwrap_i64()
    }

    fn pages(&mut self) -> i64 {
        self.pages.get(&mut self.store).unwrap_i64()
    }

    /// The browser's `instantiateCore`: debit the module's initial pages before it starts.
    fn instantiate(
        &mut self,
        module: &Module,
        initial: u64,
        extra: &[(&str, &str, Val)],
    ) -> Result<Instance, String> {
        let initial = i64::try_from(initial).unwrap();
        if self.pages() < initial {
            return Err("memory limit exceeded".into());
        }
        let remaining = self.pages() - initial;
        self.pages
            .set(&mut self.store, Val::I64(remaining))
            .unwrap();
        let mut linker = Linker::new(module.engine());
        linker
            .define(&mut self.store, LIMITS_MODULE, "fuel", self.fuel)
            .unwrap();
        linker
            .define(&mut self.store, LIMITS_MODULE, "pages", self.pages)
            .unwrap();
        for (module, name, value) in extra {
            let ty = GlobalType::new(value.ty(&self.store).unwrap(), Mutability::Const);
            let global = Global::new(&mut self.store, ty, *value).unwrap();
            linker
                .define(&mut self.store, module, name, global)
                .unwrap();
        }
        linker
            .instantiate(&mut self.store, module)
            .map_err(|error| {
                let refund = self.pages() + initial;
                self.pages.set(&mut self.store, Val::I64(refund)).unwrap();
                error.to_string()
            })
    }

    fn call<R: wasmtime::WasmResults>(
        &mut self,
        instance: &Instance,
        name: &str,
    ) -> wasmtime::Result<R> {
        instance
            .get_typed_func::<(), R>(&mut self.store, name)
            .unwrap()
            .call(&mut self.store, ())
    }
}

fn compile(engine: &Engine, wat: &str, maximum: u64) -> (Module, u64) {
    let core = instrument(&wat::parse_str(wat).unwrap(), maximum).unwrap();
    (
        Module::new(engine, &core.bytes).unwrap(),
        core.initial_pages,
    )
}

fn trap(error: wasmtime::Error) -> Trap {
    *error.downcast_ref::<Trap>().unwrap()
}

#[test]
fn repeated_instantiation_debits_each_memory_before_start() {
    let engine = engine();
    // The start function grows by one page, so each instance costs initial + 1.
    let (module, initial) = compile(
        &engine,
        r#"(module (memory 3)
        (func $start (drop (memory.grow (i32.const 1)))) (start $start)
        (func (export "size") (result i32) (memory.size)))"#,
        16,
    );
    assert_eq!(initial, 3);
    let mut limits = Limits::new(&engine, 1_000, 10);
    let first = limits.instantiate(&module, initial, &[]).unwrap();
    assert_eq!(limits.pages(), 6);
    let second = limits.instantiate(&module, initial, &[]).unwrap();
    assert_eq!(limits.pages(), 2);
    assert_eq!(limits.call::<i32>(&first, "size").unwrap(), 4);
    assert_eq!(limits.call::<i32>(&second, "size").unwrap(), 4);
    // The third instance cannot be charged its fresh memory: refused before it exists.
    assert_eq!(
        limits.instantiate(&module, initial, &[]).unwrap_err(),
        "memory limit exceeded"
    );
    assert_eq!(limits.pages(), 2);
}

#[test]
fn memory_grow_shares_one_budget_across_memories_and_memory64() {
    let engine = engine();
    let (module, initial) = compile(
        &engine,
        r#"(module (memory $a 1) (memory $b i64 1)
        (func (export "big") (result i64) (memory.grow $b (i64.const 10)))
        (func (export "small") (result i32) (memory.grow $a (i32.const 2)))
        (func (export "rest") (result i64) (memory.grow $b (i64.const 4)))
        (func (export "more") (result i32) (memory.grow $a (i32.const 1))))"#,
        8,
    );
    assert_eq!(initial, 2);
    let mut limits = Limits::new(&engine, 1_000, 8);
    let instance = limits.instantiate(&module, initial, &[]).unwrap();
    assert_eq!(limits.call::<i64>(&instance, "big").unwrap(), -1);
    assert_eq!(limits.call::<i32>(&instance, "small").unwrap(), 1);
    assert_eq!(limits.call::<i64>(&instance, "rest").unwrap(), 1);
    assert_eq!(limits.pages(), 0);
    assert_eq!(limits.call::<i32>(&instance, "more").unwrap(), -1);
}

#[test]
fn defined_memory_maximum_is_clamped() {
    let engine = engine();
    let (module, _) = compile(&engine, r#"(module (memory (export "m") 1))"#, 4);
    let memory = module
        .exports()
        .find(|export| export.name() == "m")
        .unwrap();
    let wasmtime::ExternType::Memory(memory) = memory.ty() else {
        panic!()
    };
    assert_eq!(memory.maximum(), Some(4));
}

#[test]
fn loops_and_tail_calls_exhaust_fuel() {
    let engine = engine();
    let (module, initial) = compile(
        &engine,
        r#"(module
        (func (export "spin") (loop $l (br $l)))
        (func $tail (export "tail") (return_call $tail))
        (func (export "count") (result i32) (local i32) (local.set 0 (i32.const 5))
          (loop $l (local.set 0 (i32.sub (local.get 0) (i32.const 1))) (br_if $l (local.get 0)))
          (local.get 0)))"#,
        1,
    );
    let mut limits = Limits::new(&engine, 1_000_000, 0);
    let instance = limits.instantiate(&module, initial, &[]).unwrap();
    assert_eq!(
        trap(limits.call::<()>(&instance, "spin").unwrap_err()),
        Trap::UnreachableCodeReached
    );
    assert_eq!(limits.fuel(), 0);
    limits
        .fuel
        .set(&mut limits.store, Val::I64(1_000_000))
        .unwrap();
    assert_eq!(
        trap(limits.call::<()>(&instance, "tail").unwrap_err()),
        Trap::UnreachableCodeReached
    );
    assert_eq!(limits.fuel(), 0);
    limits.fuel.set(&mut limits.store, Val::I64(100)).unwrap();
    assert_eq!(limits.call::<i32>(&instance, "count").unwrap(), 0);
    // One function entry and five loop iterations.
    assert_eq!(limits.fuel(), 94);
}

#[test]
fn defined_globals_are_remapped_behind_the_limit_imports() {
    let engine = engine();
    let (module, initial) = compile(
        &engine,
        r#"(module
        (import "env" "base" (global $base i32))
        (global $g (export "g") (mut i32) (i32.const 40))
        (global $h i32 (global.get $base))
        (memory 1) (data (i32.const 0) "\02")
        (func $start (global.set $g (i32.add (global.get $g) (i32.load8_u (i32.const 0))))) (start $start)
        (func (export "run") (result i32) (i32.add (global.get $g) (global.get $h))))"#,
        4,
    );
    let mut limits = Limits::new(&engine, 100, 4);
    let instance = limits
        .instantiate(&module, initial, &[("env", "base", Val::I32(1))])
        .unwrap();
    assert_eq!(limits.call::<i32>(&instance, "run").unwrap(), 43);
    let g = instance.get_global(&mut limits.store, "g").unwrap();
    assert_eq!(g.get(&mut limits.store).unwrap_i32(), 42);
}

#[test]
fn a_module_without_imports_receives_the_limit_imports() {
    let engine = engine();
    let (module, initial) = compile(
        &engine,
        r#"(module (func (export "one") (result i32) (i32.const 1)))"#,
        1,
    );
    let imports: Vec<_> = module
        .imports()
        .map(|import| (import.module().to_owned(), import.name().to_owned()))
        .collect();
    assert_eq!(
        imports,
        [
            (LIMITS_MODULE.to_owned(), "fuel".to_owned()),
            (LIMITS_MODULE.to_owned(), "pages".to_owned())
        ]
    );
    let mut limits = Limits::new(&engine, 10, 0);
    let instance = limits.instantiate(&module, initial, &[]).unwrap();
    assert_eq!(limits.call::<i32>(&instance, "one").unwrap(), 1);
}

#[test]
fn unsupported_or_reserved_shapes_are_refused() {
    let refuse = |wat: &str| instrument(&wat::parse_str(wat).unwrap(), 4).err().unwrap();
    assert!(
        refuse(r#"(module (import "tembra:limits" "fuel" (global (mut i64))))"#)
            .contains("reserved")
    );
    assert!(refuse(r#"(module (memory 1 2 shared))"#).contains("shared memories"));
    assert!(
        refuse(r#"(module (import "env" "m" (memory 1 2 shared)))"#).contains("shared memories")
    );
    assert!(refuse(r#"(module (memory 1 (pagesize 1)))"#).contains("page sizes"));
    assert!(refuse(r#"(module (memory 5))"#).contains("more than the limit"));
    assert!(refuse(r#"(component)"#).contains("component"));
}
