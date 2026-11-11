//! Engine-level sharing: one compiled template per component shape, one compiled module per
//! distinct core module bytes (the pinned shared source runtime included), fresh Stores.
//!
//! The rewrite itself, [`tembra_contracts::shared::split`], and the shared runtime format are
//! tested in `tembra-contracts`.
mod tests {
    use crate::{Engine, Limits};
    use std::sync::{Arc, Weak};
    use tembra_contracts::SharedRuntime;
    use tembra_contracts::shared::split;

    /// One Orb shaped like a composed source artifact. Its nested component's snapshot module
    /// defines memory and a mutable global locating `value`; its runtime module imports both and
    /// a host callback lowered from the component's `callback` import. `extra` is spliced into the
    /// nested component and `name` into a custom section.
    fn orb(value: u8, extra: &str, name: &str) -> Vec<u8> {
        wat::parse_str(format!(
            r#"(component
              (@custom "tembra-identity" "{name}")
              (import "callback" (func $callback (result u32)))
              (component $orb
                (import "callback" (func $callback (result u32)))
                (core func $lowered (canon lower (func $callback)))
                (core instance $host (export "callback" (func $lowered)))
                (core module $snapshot
                  (memory (export "memory") 1)
                  (global (export "base") (mut i32) (i32.const 8))
                  (data (i32.const 8) "\{value:02x}"))
                (core module $runtime
                  (import "env" "memory" (memory 1))
                  (import "env" "base" (global $base (mut i32)))
                  (import "host" "callback" (func $callback (result i32)))
                  (func (export "read") (result i32) (i32.load8_u (global.get $base)))
                  (func (export "bump")
                    (i32.store8 (global.get $base)
                      (i32.add (i32.load8_u (global.get $base)) (i32.const 1))))
                  (func (export "call") (result i32)
                    (i32.add (call $callback) (i32.load8_u (global.get $base)))))
                {extra}
                (core instance $env (instantiate $snapshot))
                (core instance $code (instantiate $runtime
                  (with "env" (instance $env))
                  (with "host" (instance $host))))
                (func (export "read") (result u32) (canon lift (core func $code "read")))
                (func (export "bump") (canon lift (core func $code "bump")))
                (func (export "call") (result u32) (canon lift (core func $code "call"))))
              (instance $orb (instantiate $orb (with "callback" (func $callback))))
              (export "read" (func $orb "read"))
              (export "bump" (func $orb "bump"))
              (export "call" (func $orb "call")))"#
        ))
        .unwrap()
    }

    #[test]
    fn engines_refuse_core_module_imports_they_did_not_create() {
        let engine = Engine::new(Limits::default()).unwrap();
        let original = orb(1, "", "");
        let shared = split(&original).unwrap();
        // An artifact claiming the exact name of a template import is compiled inline and
        // refused: the name is never linked for it.
        let claimed = wat::parse_str(format!(
            r#"(component (import "{}" (core module)))"#,
            shared.modules[1].name
        ))
        .unwrap();
        let refused = engine.compiled(&claimed).err().unwrap();
        assert!(refused.to_string().contains("core module"), "{refused}");
        let other = wat::parse_str(r#"(component (import "code" (core module)))"#).unwrap();
        assert!(engine.compiled(&other).is_err());
        // Malformed bytes report the real compilation error.
        assert!(engine.compiled(&original[..original.len() / 2]).is_err());
    }

    /// Instantiate one compiled Orb in a fresh Store whose host answers `callback` with
    /// `callback`, linked the way a worker links it.
    fn instantiate(
        engine: &Engine,
        compiled: &crate::engine::Compiled,
        callback: u32,
    ) -> (wasmtime::Store<()>, wasmtime::component::Instance) {
        let mut store = wasmtime::Store::new(&engine.engine, ());
        store.set_fuel(u64::MAX).unwrap();
        let mut linker = wasmtime::component::Linker::new(&engine.engine);
        compiled.link(&mut linker).unwrap();
        linker
            .root()
            .func_wrap("callback", move |_, (): ()| Ok((callback,)))
            .unwrap();
        let instance = linker.instantiate(&mut store, &compiled.component).unwrap();
        (store, instance)
    }

    fn call(
        store: &mut wasmtime::Store<()>,
        instance: &wasmtime::component::Instance,
        name: &str,
    ) -> u32 {
        let func = instance
            .get_typed_func::<(), (u32,)>(&mut *store, name)
            .unwrap();
        func.call(&mut *store, ()).unwrap().0
    }

    fn bump(store: &mut wasmtime::Store<()>, instance: &wasmtime::component::Instance) {
        let bump = instance
            .get_typed_func::<(), ()>(&mut *store, "bump")
            .unwrap();
        bump.call(&mut *store, ()).unwrap();
    }

    fn live<T>(cache: &std::sync::Mutex<std::collections::BTreeMap<[u8; 32], Weak<T>>>) -> usize {
        let cache = cache.lock().unwrap();
        cache
            .values()
            .filter(|item| item.strong_count() != 0)
            .count()
    }

    #[test]
    fn orbs_share_one_template_while_every_store_keeps_its_own_modules_and_imports() {
        for engine in [
            Engine::new(Limits::default()).unwrap(),
            Engine::interpreted(Limits::default()).unwrap(),
        ] {
            let first = engine.compiled(&orb(42, "", "first")).unwrap();
            let second = engine.compiled(&orb(7, "", "second")).unwrap();
            assert!(!Arc::ptr_eq(&first, &second));
            assert!(
                Arc::ptr_eq(&first.component, &second.component),
                "one template for both Orbs"
            );
            let [(snapshot_name, a_snapshot), (_, a_runtime)] = first.modules.as_slice() else {
                panic!("the first Orb binds its two modules");
            };
            let [(other_name, b_snapshot), (_, b_runtime)] = second.modules.as_slice() else {
                panic!("the second Orb binds its two modules");
            };
            // Equal import names, yet each Orb binds its own snapshot; the runtime is shared.
            assert_eq!(snapshot_name, other_name);
            assert!(!Arc::ptr_eq(a_snapshot, b_snapshot));
            assert!(Arc::ptr_eq(a_runtime, b_runtime));
            assert_eq!(live(&engine.templates), 1);
            assert_eq!(live(&engine.shared), 3);
            let template: Weak<wasmtime::component::Component> = Arc::downgrade(&first.component);

            let (mut one, one_instance) = instantiate(&engine, &first, 100);
            let (mut again, again_instance) = instantiate(&engine, &first, 200);
            let (mut other, other_instance) = instantiate(&engine, &second, 300);
            bump(&mut one, &one_instance);
            bump(&mut one, &one_instance);
            // Each Store starts from its own Orb's snapshot and mutates only its own memory.
            assert_eq!(call(&mut one, &one_instance, "read"), 44);
            assert_eq!(call(&mut again, &again_instance, "read"), 42);
            assert_eq!(call(&mut other, &other_instance, "read"), 7);
            // Each Store's runtime calls only its own Store's host import.
            assert_eq!(call(&mut one, &one_instance, "call"), 144);
            assert_eq!(call(&mut again, &again_instance, "call"), 242);
            assert_eq!(call(&mut other, &other_instance, "call"), 307);

            // Live Stores keep running after the compilations leave their owners.
            drop((first, second));
            assert_eq!(call(&mut other, &other_instance, "read"), 7);
            drop((one, again, other));
            assert!(
                template.upgrade().is_none(),
                "templates are released with their users"
            );
            assert_eq!(live(&engine.templates), 0);
            assert_eq!(live(&engine.shared), 0);
        }
    }

    #[test]
    fn unsupported_shapes_still_run_inline() {
        let engine = Engine::new(Limits::default()).unwrap();
        let late = engine
            .compiled(&orb(5, "(core type (module))", ""))
            .unwrap();
        assert!(late.modules.is_empty());
        let (mut store, instance) = instantiate(&engine, &late, 10);
        assert_eq!(call(&mut store, &instance, "call"), 15);
        assert_eq!(live(&engine.templates), 0);
        assert_eq!(live(&engine.shared), 0);
    }

    /// Real source Orbs, which import the pinned runtime: on Cranelift and Pulley every Orb
    /// links the one compiled runtime module of its Engine, Orbs of one world share a template,
    /// and an import of the runtime's name with another type fails Wasmtime's link check.
    #[test]
    #[ignore = "requires TEMBRA_WASM_FIXTURES built against the pinned runtime"]
    fn packed_source_orbs_link_one_runtime_module_per_engine() {
        let directory = std::env::var_os("TEMBRA_WASM_FIXTURES").expect("TEMBRA_WASM_FIXTURES");
        let mut thin = Vec::new();
        let mut paths: Vec<_> = std::fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "wasm")
            })
            .collect();
        paths.sort();
        for path in paths {
            let bytes = std::fs::read(&path).unwrap();
            let Ok(artifact) = tembra_contracts::Artifact::decode(&bytes) else {
                continue;
            };
            if !artifact.source_runtime {
                continue;
            }
            assert!(artifact.shared_runtime, "{path:?}");
            thin.push(bytes);
        }
        assert!(thin.len() >= 2, "at least two source fixtures");
        // Two Orbs of one world: same template, different bytes.
        let template = |bytes: &[u8]| split(bytes).unwrap().component;
        let (first, second) = thin
            .iter()
            .enumerate()
            .find_map(|(index, first)| {
                thin[index + 1..]
                    .iter()
                    .find(|second| template(first) == template(second))
                    .map(|second| (first, second))
            })
            .expect("two source fixtures of one world");
        assert_ne!(first, second);
        let forged = wat::parse_str(format!(
            r#"(component (import "tembra-runtime-core-h{}" (core module)))"#,
            SharedRuntime::SHA256
        ))
        .unwrap();
        for engine in [
            Engine::new(Limits::default()).unwrap(),
            Engine::interpreted(Limits::default()).unwrap(),
        ] {
            let compiled = thin
                .iter()
                .map(|bytes| engine.compiled(bytes).unwrap())
                .collect::<Vec<_>>();
            let pinned = |compiled: &crate::engine::Compiled| {
                let runtimes = compiled
                    .modules
                    .iter()
                    .filter(|(name, _)| tembra_contracts::shared_runtime_import(name))
                    .map(|(_, module)| module.clone())
                    .collect::<Vec<_>>();
                assert_eq!(runtimes.len(), 1);
                runtimes[0].clone()
            };
            let runtime = pinned(&compiled[0]);
            for compiled in &compiled {
                assert!(Arc::ptr_eq(&pinned(compiled), &runtime));
            }
            let (one, two) = (
                engine.compiled(first).unwrap(),
                engine.compiled(second).unwrap(),
            );
            assert!(Arc::ptr_eq(&one.component, &two.component));
            assert!(!Arc::ptr_eq(&one, &two));
            // The pinned name with another module type: compiled, linked, refused by Wasmtime.
            let forged = engine.compiled(&forged).unwrap();
            let mut store = wasmtime::Store::new(&engine.engine, ());
            let mut linker = wasmtime::component::Linker::new(&engine.engine);
            forged.link(&mut linker).unwrap();
            assert!(linker.instantiate(&mut store, &forged.component).is_err());
            drop((compiled, one, two, forged, store, linker, runtime));
            assert_eq!(live(&engine.shared), 0);
            assert_eq!(live(&engine.templates), 0);
        }
    }
}
