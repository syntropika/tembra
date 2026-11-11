//! Component rewrites around the build-time snapshot.
//!
//! Ported from componentize-qjs v0.6.0 (commit 758687536ea711ede17369f53a90ddc3e97a8dbb),
//! `crates/core/src/postprocess.rs` (`prepare_snapshot`) and `crates/core/src/stubwasi.rs`,
//! licensed under the Apache License, Version 2.0. Tembra changes: the stub world imports
//! whatever retained interfaces a stubbed interface uses for types, so a stub never shadows an
//! interface the contract imports; size-only post-processing is not ported.

use std::collections::BTreeSet;

use anyhow::{Context, Result, bail, ensure};
use wasm_compose::{composer::ComponentComposer, config::Config as ComposeConfig};
use wasm_encoder::reencode::{ReencodeComponent, RoundtripReencoder};
use wasm_encoder::{Component, RawSection};
use wasmparser::{ComponentExternalKind, Parser, Payload};
use wit_component::{ComponentEncoder, StringEncoding, dummy_module, embed_component_metadata};
use wit_parser::decoding::{DecodedWasm, decode};
use wit_parser::{
    Docs, InterfaceId, ManglingAndAbi, Resolve, Stability, World, WorldId, WorldItem, WorldKey,
};

/// Give the runtime's `init` export a globally unused name so Wizer removes only it.
pub(super) fn prepare_snapshot(input: &[u8]) -> Result<(Vec<u8>, String)> {
    ensure!(Parser::is_component(input), "expected a wasm component");
    let mut names = BTreeSet::new();
    for payload in Parser::new(0).parse_all(input) {
        match payload? {
            Payload::ExportSection(exports) => {
                for export in exports {
                    names.insert(export?.name);
                }
            }
            Payload::ComponentExportSection(exports) => {
                for export in exports {
                    names.insert(export?.name.name);
                }
            }
            _ => {}
        }
    }
    let mut name = String::from("componentize-qjs-init");
    while names.contains(name.as_str()) {
        name.push_str("-x");
    }

    let mut output = Component::new();
    let mut found = false;
    for (payload, id, data) in sections(input)? {
        let Payload::ComponentExportSection(exports) = payload else {
            output.section(&RawSection { id, data });
            continue;
        };
        let mut section = wasm_encoder::ComponentExportSection::new();
        for export in exports {
            let mut export = export?;
            if export.name.name == "init" {
                ensure!(!found, "duplicate component init export");
                ensure!(
                    export.kind == ComponentExternalKind::Func,
                    "component init export must be a function"
                );
                found = true;
                export.name.name = &name;
            }
            RoundtripReencoder.parse_component_export(&mut section, export)?;
        }
        output.section(&section);
    }
    ensure!(found, "missing component init export");
    Ok((output.finish(), name))
}

/// Top-level sections as `(parsed payload, section ID, raw contents)`.
fn sections(input: &[u8]) -> Result<Vec<(Payload<'_>, u8, &[u8])>> {
    let mut sections = Vec::new();
    let mut depth = 0;
    for payload in Parser::new(0).parse_all(input) {
        let payload = payload?;
        let top_level = depth == 0;
        match payload {
            Payload::ModuleSection { .. } | Payload::ComponentSection { .. } => depth += 1,
            Payload::End(_) if !top_level => depth -= 1,
            _ => {}
        }
        if top_level && let Some((id, range)) = payload.as_section() {
            sections.push((
                payload,
                id,
                &input[range.start as usize..range.end as usize],
            ));
        }
    }
    Ok(sections)
}

/// Decode the component's actual world; embedded metadata is never trusted.
pub(super) fn world(component: &[u8]) -> Result<(Resolve, WorldId)> {
    match decode(component).context("failed to decode component WIT")? {
        DecodedWasm::Component(resolve, world) => Ok((resolve, world)),
        DecodedWasm::WitPackage(..) => bail!("expected a component, got a WIT package"),
    }
}

/// Satisfy the selected imports with trapping implementations composed into the component.
pub(super) fn stub_imports(component: &[u8], stubbed: &BTreeSet<String>) -> Result<Vec<u8>> {
    if stubbed.is_empty() {
        return Ok(component.to_vec());
    }
    let (resolve, world) = world(component)?;
    let original = &resolve.worlds[world];
    let exports: indexmap::IndexMap<WorldKey, WorldItem> = original
        .imports
        .iter()
        .filter(|(key, _)| stubbed.contains(&resolve.name_world_key(key)))
        .map(|(key, item)| (key.clone(), item.clone()))
        .collect();
    ensure!(
        exports.len() == stubbed.len(),
        "a stubbed import is not imported by the component"
    );
    // Retained interfaces whose types a stub uses stay imports of the stub itself.
    let stubbed_ids: BTreeSet<InterfaceId> = exports
        .values()
        .filter_map(|item| match item {
            WorldItem::Interface { id, .. } => Some(*id),
            _ => None,
        })
        .collect();
    let imports = original
        .imports
        .iter()
        .filter(|(_, item)| match item {
            WorldItem::Interface { id, .. } => {
                !stubbed_ids.contains(id)
                    && stubbed_ids.iter().any(|stub| uses(&resolve, *stub, *id))
            }
            _ => false,
        })
        .map(|(key, item)| (key.clone(), item.clone()))
        .collect();

    let mut stub_resolve = resolve.clone();
    let stub_world = stub_resolve.worlds.alloc(World {
        name: "tembra-implicit-stubs".into(),
        imports,
        exports,
        package: original.package,
        docs: Docs::default(),
        stability: Stability::default(),
        includes: Vec::new(),
        span: Default::default(),
    });
    let mut module = dummy_module(&stub_resolve, stub_world, ManglingAndAbi::Standard32);
    embed_component_metadata(
        &mut module,
        &stub_resolve,
        stub_world,
        StringEncoding::UTF8,
        false,
    )
    .context("failed to embed stub metadata")?;
    let stubs = ComponentEncoder::default()
        .module(&module)?
        .validate(true)
        .encode()
        .context("failed to encode stub component")?;

    let dir = tempfile::tempdir().context("failed to create composition directory")?;
    let original_path = dir.path().join("original.wasm");
    std::fs::write(&original_path, component)?;
    std::fs::write(dir.path().join("stubs.wasm"), stubs)?;
    let config = ComposeConfig {
        dir: dir.path().to_path_buf(),
        definitions: vec!["stubs.wasm".into()],
        ..Default::default()
    };
    ComponentComposer::new(&original_path, &config)
        .compose()
        .context("failed to compose implicit import stubs")
}

/// Whether `interface` transitively uses types from `dependency`.
fn uses(resolve: &Resolve, interface: InterfaceId, dependency: InterfaceId) -> bool {
    let mut pending = vec![interface];
    let mut seen = BTreeSet::new();
    while let Some(next) = pending.pop() {
        for direct in resolve.interface_direct_deps(next) {
            if direct == dependency {
                return true;
            }
            if seen.insert(direct) {
                pending.push(direct);
            }
        }
    }
    false
}
