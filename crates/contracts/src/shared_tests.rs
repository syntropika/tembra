//! The module rewrites: engine templates (`shared::split`) and the shared source runtime
//! (`SharedRuntime::pack`, `expand` and the reference check every decode runs).
use crate::shared::*;
use crate::shared_runtime::{FORWARD, LOCAL};
use crate::{Artifact, Contract, SharedRuntime};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use wasm_encoder::{ComponentExportKind, ComponentInstanceSection, RawSection};
use wasmparser::ComponentTypeRef as Ref;
use wasmparser::{BinaryReader, ComponentExternalKind, ComponentInstance, Validator, WasmFeatures};

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

/// The `(name, type)` of each import at one level of a component.
fn imports(bytes: &[u8]) -> Vec<(String, Ref)> {
    let mut found = Vec::new();
    for (id, data) in sections(bytes).unwrap() {
        if id == IMPORT {
            let reader =
                wasmparser::ComponentImportSectionReader::new(BinaryReader::new(data, 0)).unwrap();
            for import in reader {
                let import = import.unwrap();
                found.push((import.name.name.to_owned(), import.ty));
            }
        }
    }
    found
}

/// The module imports of one level, by name.
fn module_imports(bytes: &[u8]) -> Vec<String> {
    imports(bytes)
        .into_iter()
        .filter(|(_, ty)| matches!(ty, Ref::Module(_)))
        .map(|(name, _)| name)
        .collect()
}

/// The single nested component definition of a level.
fn nested(bytes: &[u8]) -> &[u8] {
    let mut nested = sections(bytes)
        .unwrap()
        .into_iter()
        .filter(|(id, _)| *id == COMPONENT);
    let (_, data) = nested.next().unwrap();
    assert!(nested.next().is_none());
    data
}

/// The module-kind instantiation argument names of one level.
fn forwarded(bytes: &[u8]) -> Vec<String> {
    let mut forwarded = Vec::new();
    for (id, data) in sections(bytes).unwrap() {
        if id == INSTANCE {
            let reader =
                wasmparser::ComponentInstanceSectionReader::new(BinaryReader::new(data, 0))
                    .unwrap();
            for instance in reader {
                if let ComponentInstance::Instantiate { args, .. } = instance.unwrap() {
                    forwarded.extend(
                        args.iter()
                            .filter(|arg| arg.kind == ComponentExternalKind::Module)
                            .map(|arg| arg.name.to_owned()),
                    );
                }
            }
        }
    }
    forwarded
}

fn validate(bytes: &[u8]) {
    Validator::new_with_features(WasmFeatures::all())
        .validate_all(bytes)
        .unwrap();
}

#[test]
fn every_module_becomes_an_import_forwarded_through_every_component() {
    let original = orb(42, "", "first");
    let split = split(&original).expect("the modules are hoisted");
    validate(&split.component);
    assert!(split.runtime.is_empty());
    let [snapshot, runtime] = split.modules.as_slice() else {
        panic!("expected the snapshot and runtime modules");
    };
    for module in [snapshot, runtime] {
        assert!(module.name.starts_with(PREFIX));
        assert_eq!(module.hash, <[u8; 32]>::from(Sha256::digest(module.bytes)));
        // The name is derived from the module's own type, never from its bytes.
        assert_eq!(module.name, name(&describe(module.bytes).unwrap().key));
    }
    assert_ne!(snapshot.name, runtime.name);
    // The root imports both modules and passes them to the component that embedded them,
    // which imports them under the same names at the defining modules' positions.
    let names = vec![snapshot.name.clone(), runtime.name.clone()];
    assert_eq!(module_imports(&split.component), names);
    assert_eq!(module_imports(nested(&split.component)), names);
    assert_eq!(forwarded(&split.component), names);
    // No custom section survives at any level.
    for level in [split.component.as_slice(), nested(&split.component)] {
        assert!(sections(level).unwrap().iter().all(|(id, _)| *id != CUSTOM));
    }
}

#[test]
fn orbs_that_differ_only_in_module_bytes_and_metadata_rewrite_to_one_template() {
    let first = orb(42, "", "first");
    let second = orb(7, "", "second");
    assert_ne!(first, second);
    let (first, second) = (split(&first).unwrap(), split(&second).unwrap());
    assert_eq!(first.component, second.component);
    // Same names, but each binds its own snapshot bytes.
    let names = |split: &Split<'_>| -> Vec<String> {
        split
            .modules
            .iter()
            .map(|module| module.name.clone())
            .collect()
    };
    assert_eq!(names(&first), names(&second));
    assert_ne!(first.modules[0].hash, second.modules[0].hash);
    assert_eq!(first.modules[1].hash, second.modules[1].hash);
    // A different module type is a different template.
    let other = orb(42, "(core module (func (export \"f\")))", "first");
    assert_ne!(split(&other).unwrap().component, first.component);
}

/// Whether `wat` is rewritten.
fn rewritten(wat: &str) -> bool {
    split(&wat::parse_str(wat).unwrap()).is_some()
}

#[test]
fn unsupported_or_malformed_shapes_are_never_rewritten() {
    // Each refused shape below differs from a rewritten control in one item only.
    assert!(rewritten(
        r#"(component
          (component $inner (core module (func nop)))
          (instance (instantiate $inner)))"#
    ));
    assert!(rewritten(
        r#"(component (core module (func nop)) (core module (func (export "f") nop)))"#
    ));
    assert!(rewritten(
        r#"(component (import "named" (func)) (core module (func nop)))"#
    ));
    assert!(split(&orb(1, "", "")).is_some());
    // A core type defined after a hoisted module would be renumbered.
    assert!(split(&orb(1, "(core type (module))", "")).is_none());
    // A module defined after an import inserted for a nested component would be renumbered.
    assert!(!rewritten(
        r#"(component
          (component $inner (core module (func nop)))
          (instance (instantiate $inner))
          (core module))"#
    ));
    // One level cannot import the same module twice.
    assert!(!rewritten(
        r#"(component (core module (func nop)) (core module (func nop)))"#
    ));
    // One name never selects two modules of one type, at one level or across levels.
    assert!(!rewritten(
        r#"(component (core module (func nop)) (core module (func nop nop)))"#
    ));
    assert!(!rewritten(
        r#"(component
          (core module (func nop))
          (component $inner (core module (func nop nop)))
          (instance (instantiate $inner)))"#
    ));
    // A component never chooses the reserved names, nor imports a core module at its root
    // other than the exact pinned runtime.
    assert!(!rewritten(
        r#"(component (import "tembra-shared-core-mab" (func)) (core module (func nop)))"#
    ));
    assert!(!rewritten(
        r#"(component (import "tembra-runtime-core-hab" (func)) (core module (func nop)))"#
    ));
    assert!(!rewritten(
        r#"(component (import "code" (core module)) (core module (func nop)))"#
    ));
    // Nothing to share, truncated or non-component bytes.
    assert!(!rewritten("(component)"));
    let original = orb(1, "", "");
    assert!(split(&original[..original.len() / 2]).is_none());
    assert!(split(b"\0asm\x01\0\0\0").is_none());
    assert!(split(b"\0asm").is_none());
}

/// A component of `sections`, each `(id, contents)`.
fn component(sections: &[(u8, &[u8])]) -> Vec<u8> {
    let mut out = wasm_encoder::Component::new();
    for (id, data) in sections {
        out.section(&RawSection { id: *id, data });
    }
    out.finish()
}

/// An instance section instantiating component 0 with `args`.
fn instantiate(args: &[(&str, ComponentExportKind, u32)]) -> Vec<u8> {
    let mut section = ComponentInstanceSection::new();
    section.instantiate(0, args.iter().copied());
    let bytes = {
        let mut out = wasm_encoder::Component::new();
        out.section(&section);
        out.finish()
    };
    sections(&bytes).unwrap()[0].1.to_vec()
}

fn runtime() -> &'static SharedRuntime {
    SharedRuntime::pinned()
}

/// The pinned runtime defined two components deep (never instantiated) next to a small module,
/// with custom sections at every level, like a composed source artifact.
fn fat() -> Vec<u8> {
    let small = wat::parse_str("(module (func (export \"f\")))").unwrap();
    let inner = component(&[
        (CUSTOM, b"\x05inner"),
        (MODULE, &small),
        (MODULE, runtime().bytes()),
    ]);
    let middle = component(&[(COMPONENT, &inner), (INSTANCE, &instantiate(&[]))]);
    component(&[
        (CUSTOM, b"\x04root"),
        (COMPONENT, &middle),
        (INSTANCE, &instantiate(&[])),
        (CUSTOM, b"\x04tail"),
    ])
}

#[test]
fn the_pinned_runtime_is_checked_by_content_and_type() {
    let runtime = runtime();
    assert_eq!(
        crate::shared::hex(&Sha256::digest(runtime.bytes())),
        SharedRuntime::SHA256
    );
    assert_eq!(runtime.bytes().len(), 1_976_947);
    assert!(SharedRuntime::new(runtime.bytes().to_vec()).is_ok());
    let mut other = runtime.bytes().to_vec();
    *other.last_mut().unwrap() ^= 1;
    let error = SharedRuntime::new(other).err().unwrap();
    assert!(
        error.0.contains("is not the pinned source runtime"),
        "{error}"
    );
    assert!(LOCAL.ends_with(SharedRuntime::SHA256));
    assert!(FORWARD.ends_with(SharedRuntime::SHA256));
}

#[test]
fn packing_imports_the_runtime_through_every_level_and_expands_back_exactly() {
    let fat = fat();
    validate(&fat);
    let thin = runtime().pack(&fat).unwrap();
    validate(&thin);
    assert!(thin.len() < fat.len() / 100, "{} bytes", thin.len());
    assert!(crate::shared_runtime::references(&thin).unwrap());
    // The root and the middle component forward, the defining component imports it in place.
    let middle = nested(&thin);
    let inner = nested(middle);
    assert_eq!(module_imports(&thin), [FORWARD]);
    assert_eq!(module_imports(middle), [FORWARD]);
    assert_eq!(module_imports(inner), [LOCAL]);
    assert_eq!(forwarded(&thin), [FORWARD]);
    assert_eq!(forwarded(middle), [LOCAL]);
    // Every custom section and the other module stay.
    for (level, expected) in [(&thin[..], 2), (inner, 1)] {
        let customs = sections(level).unwrap();
        assert_eq!(
            customs.iter().filter(|(id, _)| *id == CUSTOM).count(),
            expected
        );
    }
    assert_eq!(runtime().expand(&thin).unwrap(), fat);
    // Packing is not repeated, and needs the exact runtime bytes.
    assert!(runtime().pack(&thin).is_err());
    let absent = orb(1, "", "");
    let error = runtime().pack(&absent).err().unwrap();
    assert!(error.0.contains("does not embed the pinned"), "{error}");
    // Engines keep the root runtime import and hoist every other module into the template.
    let template = split(&thin).unwrap();
    assert_eq!(template.runtime, [FORWARD]);
    assert_eq!(template.modules.len(), 1);
    validate(&template.component);
}

/// The pinned module type section, as packing writes it.
fn pinned_type() -> Vec<u8> {
    type_section(&describe(runtime().bytes()).unwrap().ty)
}

#[test]
fn repeated_runtime_definitions_cannot_amplify_expansion() {
    let types = pinned_type();
    let local = import_section(LOCAL, wasm_encoder::ComponentTypeRef::Module(0));
    let child = component(&[(CORE_TYPE, &types), (IMPORT, &local)]);
    let forward = import_section(FORWARD, wasm_encoder::ComponentTypeRef::Module(0));
    let mut instances = ComponentInstanceSection::new();
    instances.instantiate(0, [(LOCAL, ComponentExportKind::Module, 0)]);
    instances.instantiate(1, [(LOCAL, ComponentExportKind::Module, 0)]);
    let mut wrapper = wasm_encoder::Component::new();
    wrapper.section(&instances);
    let encoded = wrapper.finish();
    let instances = sections(&encoded).unwrap()[0].1;
    let thin = component(&[
        (CORE_TYPE, &types),
        (IMPORT, &forward),
        (COMPONENT, &child),
        (COMPONENT, &child),
        (INSTANCE, instances),
    ]);
    validate(&thin);
    assert!(crate::shared_runtime::references(&thin).is_err());
    assert!(runtime().expand(&thin).is_err());
}

/// A single-entry import section.
fn import_section(name: &str, ty: wasm_encoder::ComponentTypeRef) -> Vec<u8> {
    let mut section = wasm_encoder::ComponentImportSection::new();
    section.import(name, ty);
    let bytes = {
        let mut out = wasm_encoder::Component::new();
        out.section(&section);
        out.finish()
    };
    sections(&bytes).unwrap()[0].1.to_vec()
}

fn refused(bytes: &[u8], expected: &str) {
    let error = crate::shared_runtime::references(bytes).err().unwrap();
    assert!(error.0.contains(expected), "{error}");
    assert!(runtime().expand(bytes).is_err());
}

#[test]
fn forged_missing_and_mismatched_references_are_refused() {
    let ty = pinned_type();
    let module = wasm_encoder::ComponentTypeRef::Module(0);
    // The exact reference is accepted at the root.
    let exact = component(&[(CORE_TYPE, &ty), (IMPORT, &import_section(LOCAL, module))]);
    assert!(crate::shared_runtime::references(&exact).unwrap());
    // Another runtime's digest names a module this host does not have.
    let other = LOCAL.replace(SharedRuntime::SHA256, &"0".repeat(64));
    refused(
        &component(&[(CORE_TYPE, &ty), (IMPORT, &import_section(&other, module))]),
        "requires runtime core 0000",
    );
    // The pinned name with another module type, or without the type section before it.
    let empty = wat::parse_str(format!(r#"(component (import "{LOCAL}" (core module)))"#)).unwrap();
    refused(&empty, "without its pinned type");
    let mut wrong = wasm_encoder::CoreTypeSection::new();
    wrong.ty().module(&wasm_encoder::ModuleType::new());
    let wrong = {
        let mut out = wasm_encoder::Component::new();
        out.section(&wrong);
        out.finish()
    };
    let wrong = sections(&wrong).unwrap()[0].1.to_vec();
    refused(
        &component(&[
            (CORE_TYPE, &wrong),
            (IMPORT, &import_section(LOCAL, module)),
        ]),
        "without its pinned type",
    );
    // Any other root core-module import.
    refused(
        &wat::parse_str(r#"(component (import "code" (core module)))"#).unwrap(),
        "imports core module `code`",
    );
    // A forwarded runtime that no nested component imports.
    refused(
        &component(&[(CORE_TYPE, &ty), (IMPORT, &import_section(FORWARD, module))]),
        "no component imports",
    );
    // A nested runtime import supplied with anything but the parent's own runtime import.
    let small = wat::parse_str("(module)").unwrap();
    let inner = component(&[(CORE_TYPE, &ty), (IMPORT, &import_section(LOCAL, module))]);
    refused(
        &component(&[
            (MODULE, &small),
            (COMPONENT, &inner),
            (
                INSTANCE,
                &instantiate(&[(LOCAL, ComponentExportKind::Module, 0)]),
            ),
        ]),
        "not this component's own runtime",
    );
    // Two runtime imports in one component, and a core type after the runtime type.
    refused(
        &component(&[
            (CORE_TYPE, &ty),
            (IMPORT, &import_section(LOCAL, module)),
            (CORE_TYPE, &ty),
            (
                IMPORT,
                &import_section(LOCAL, wasm_encoder::ComponentTypeRef::Module(1)),
            ),
        ]),
        "two runtime imports",
    );
    refused(
        &component(&[
            (CORE_TYPE, &ty),
            (IMPORT, &import_section(LOCAL, module)),
            (CORE_TYPE, &wrong),
        ]),
        "core type follows",
    );
    // Nothing but the runtime import may use the runtime's type, which expansion removes and
    // engine templates renumber: not another import, nor a nested component's alias.
    let reused = component(&[
        (CORE_TYPE, &ty),
        (IMPORT, &import_section(LOCAL, module)),
        (IMPORT, &import_section("other", module)),
    ]);
    refused(
        &component(&[(COMPONENT, &reused)]),
        "another import uses the runtime's type",
    );
    let mut alias = wasm_encoder::ComponentAliasSection::new();
    alias.alias(wasm_encoder::Alias::Outer {
        kind: wasm_encoder::ComponentOuterAliasKind::CoreType,
        count: 1,
        index: 0,
    });
    let alias = {
        let mut out = wasm_encoder::Component::new();
        out.section(&alias);
        out.finish()
    };
    let aliasing = component(&[(ALIAS, sections(&alias).unwrap()[0].1)]);
    refused(
        &component(&[
            (CORE_TYPE, &ty),
            (IMPORT, &import_section(LOCAL, module)),
            (COMPONENT, &aliasing),
        ]),
        "aliases the runtime's type",
    );
    // Without any reference there is nothing to expand.
    let error = runtime().expand(&orb(1, "", "")).err().unwrap();
    assert!(error.0.contains("does not reference"), "{error}");
}

const HOOKS: &str = r#"
package example:hooks@1.0.0;
interface guard { apply: func(input: u64) -> u64; }
world provider { export guard; }
"#;

/// A real component implementing `world` with the pinned runtime defined, uninstantiated, at its
/// root after every other item.
fn with_runtime(world: &Contract) -> Vec<u8> {
    let resolve = &world.resolve;
    let mut module =
        wit_component::dummy_module(resolve, world.world, wit_parser::ManglingAndAbi::Standard32);
    wit_component::embed_component_metadata(
        &mut module,
        resolve,
        world.world,
        wit_component::StringEncoding::UTF8,
        false,
    )
    .unwrap();
    let mut bytes = wit_component::ComponentEncoder::default()
        .module(&module)
        .unwrap()
        .validate(true)
        .encode()
        .unwrap();
    let mut extra = wasm_encoder::Component::new();
    extra.section(&RawSection {
        id: MODULE,
        data: runtime().bytes(),
    });
    bytes.extend_from_slice(&extra.finish()[8..]);
    bytes
}

#[test]
fn source_artifacts_may_import_the_runtime_and_compiled_artifacts_may_not() {
    let public = Contract::from_source("hooks.wit", HOOKS, Some("provider")).unwrap();
    let fat = with_runtime(&public.source_world(&BTreeSet::new()).unwrap());
    let thin = runtime().pack(&fat).unwrap();
    assert_eq!(module_imports(&thin), [LOCAL]);
    // The thin component's actual WIT is verified without the runtime bytes.
    let packaged = public
        .source_component_artifact(&thin, "guarded", 1)
        .unwrap();
    let artifact = Artifact::decode(&packaged).unwrap();
    assert!(artifact.source_runtime && artifact.shared_runtime);
    assert_eq!(artifact.component, packaged);
    assert_eq!(
        runtime().expand(&packaged).unwrap(),
        [&fat[..], &packaged[thin.len()..]].concat()
    );
    // A source component that embeds its runtime is refused, packaged or forged.
    public.check_source_component(&fat).unwrap();
    let error = public
        .source_component_artifact(&fat, "embedded", 1)
        .err()
        .unwrap();
    assert!(error.0.contains("must import the pinned shared"), "{error}");
    let mut embedded = fat.clone();
    let identity = crate::Identity {
        name: "embedded".into(),
        api_version: 1,
        world: "provider".into(),
    };
    crate::artifact::append_section(
        &mut embedded,
        "tembra:orb",
        &crate::artifact::encode_orb(&identity, public.prepared()),
    );
    crate::artifact::append_section(
        &mut embedded,
        "tembra:runtime",
        crate::SOURCE_PACKAGE.as_bytes(),
    );
    let error = Artifact::decode(&embedded).err().unwrap();
    assert!(error.0.contains("must import the pinned shared"), "{error}");
    // The same reference renamed to another runtime is refused before anything runs.
    let digest = SharedRuntime::SHA256.as_bytes();
    let mut other = packaged.clone();
    let position = other
        .windows(digest.len())
        .position(|window| window == digest)
        .unwrap();
    other[position] = if other[position] == b'0' { b'1' } else { b'0' };
    let error = Artifact::decode(&other).err().unwrap();
    assert!(error.0.contains("requires runtime core"), "{error}");
    // A compiled artifact may import no core module, the pinned runtime included.
    let compiled = runtime().pack(&with_runtime(&public)).unwrap();
    let error = public
        .component_artifact(&compiled, "compiled", 1, &BTreeMap::new())
        .err()
        .unwrap();
    assert!(error.0.contains("self-contained"), "{error}");
    let mut forged = compiled;
    crate::artifact::append_section(
        &mut forged,
        "tembra:orb",
        &crate::artifact::encode_orb(
            &crate::Identity {
                name: "forged".into(),
                api_version: 1,
                world: "provider".into(),
            },
            public.prepared(),
        ),
    );
    let error = Artifact::decode(&forged).err().unwrap();
    assert!(error.0.contains("self-contained"), "{error}");
}

/// Every packaged source fixture imports the pinned runtime, is verified from its actual thin
/// types, expands to a component that packs back to it exactly, and still rewrites to a template.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn source_fixtures_verify_thin_and_round_trip_exactly() {
    let directory = std::env::var_os("TEMBRA_WASM_FIXTURES").expect("TEMBRA_WASM_FIXTURES");
    let mut checked = 0;
    // Templates group Orbs that differ only in module bytes; sharing must not split a group.
    let (mut fat_templates, mut thin_templates) = (BTreeSet::new(), BTreeSet::new());
    let mut entries: Vec<_> = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "wasm")
        })
        .collect();
    entries.sort();
    for path in entries {
        let thin = std::fs::read(&path).unwrap();
        let Ok(container) = crate::artifact::Container::parse(&thin) else {
            continue;
        };
        if !container.tembra.contains_key("tembra:runtime") {
            continue;
        }
        let artifact = Artifact::decode(&thin).unwrap_or_else(|error| panic!("{path:?}: {error}"));
        assert!(artifact.source_runtime && artifact.shared_runtime);
        let fat = runtime().expand(&thin).unwrap();
        validate(&fat);
        assert_eq!(runtime().pack(&fat).unwrap(), thin, "{path:?}");
        let template = split(&thin).unwrap_or_else(|| panic!("{path:?}: no template"));
        validate(&template.component);
        assert_eq!(template.runtime.len(), 1);
        thin_templates.insert(Sha256::digest(&template.component));
        fat_templates.insert(Sha256::digest(&split(&fat).unwrap().component));
        println!("{path:?}: {} -> {}", fat.len(), thin.len());
        checked += 1;
    }
    assert!(checked > 0);
    assert_eq!(thin_templates.len(), fat_templates.len());
    println!(
        "{checked} source fixtures, {} templates",
        thin_templates.len()
    );
}

/// A test component given a legitimate shared runtime reference: the pinned runtime defined,
/// uninstantiated, at its root, then packed.
pub(crate) fn thin(mut component: Vec<u8>) -> Vec<u8> {
    let mut extra = wasm_encoder::Component::new();
    extra.section(&RawSection {
        id: MODULE,
        data: runtime().bytes(),
    });
    component.extend_from_slice(&extra.finish()[8..]);
    runtime().pack(&component).unwrap()
}
