//! Hoist core modules out of a component without renumbering any index space.
//!
//! Two rewrites share this code:
//!
//! - [`split`] (engines): every core module, at any nesting depth, becomes a core-module import
//!   forwarded through each enclosing component as an instantiation argument. The import is named
//!   after the SHA-256 of the module's encoded type and component custom sections are dropped,
//!   so Orbs that differ only in module bytes (a snapshot's data, say) rewrite to one template.
//!   An engine compiles each template once and each module once per distinct bytes. A root
//!   import of the pinned shared runtime ([`crate::SharedRuntime`]) is kept and reported.
//! - [`pack`] (source builds): only the module whose bytes equal the pinned runtime core is
//!   hoisted, under the runtime's content-addressed names, and every other byte, custom sections
//!   included, is kept. The result is a standard thin component (see `shared_runtime`).
//!
//! Nothing in the component (a custom section, a name or a claimed hash) identifies a module:
//! every name and hash is computed here from the actual bytes, a component that already uses a
//! reserved name is never rewritten (except for the exact pinned runtime import of a thin
//! artifact) and one name never selects two different modules.
//!
//! The rewrite never renumbers an index space. A hoisted module becomes an import at its own
//! position, so its module index is unchanged. A module a nested component needs is imported just
//! before that component only when nothing later at that level adds a core module; likewise a
//! module type is inserted only when nothing later adds a core type. Every other shape returns
//! `None`.
use crate::shared_runtime::{FORWARD, LOCAL, RESERVED, pattern};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use wasm_encoder::reencode::{Reencode, ReencodeComponent, RoundtripReencoder};
use wasm_encoder::{
    ComponentExportKind, ComponentImportSection, ComponentInstanceSection, ComponentTypeRef,
    CoreTypeSection, Encode, EntityType, ModuleType, RawSection,
};
use wasmparser::{
    BinaryReader, ComponentAlias, ComponentExternalKind, ComponentInstance,
    ComponentOuterAliasKind, CoreType, Encoding, ExternalKind, Parser, Payload, TypeRef,
};

/// Prefix of the template import names [`split`] creates; a component may never use it itself.
pub(crate) const PREFIX: &str = "tembra-shared-core-m";

pub(crate) const CUSTOM: u8 = 0;
pub(crate) const MODULE: u8 = 1;
pub(crate) const CORE_INSTANCE: u8 = 2;
pub(crate) const CORE_TYPE: u8 = 3;
pub(crate) const COMPONENT: u8 = 4;
pub(crate) const INSTANCE: u8 = 5;
pub(crate) const ALIAS: u8 = 6;
pub(crate) const TYPE: u8 = 7;
pub(crate) const CANONICAL: u8 = 8;
pub(crate) const START: u8 = 9;
pub(crate) const IMPORT: u8 = 10;
pub(crate) const EXPORT: u8 = 11;

/// A component rewritten into a template that imports all of its core modules.
pub struct Split<'a> {
    /// The template; its only additional top-level imports are `modules`.
    pub component: Vec<u8>,
    /// Each distinct hoisted module, in import order.
    pub modules: Vec<Hoisted<'a>>,
    /// The pinned shared-runtime import names the component itself has at its root; the engine
    /// links its own copy of the pinned runtime for each.
    pub runtime: Vec<&'static str>,
}

/// One hoisted core module and the import name the template uses for it.
pub struct Hoisted<'a> {
    /// Derived from the module's type; equal for modules of equal type in other components.
    pub name: String,
    /// SHA-256 of `bytes`, which keys the module's compiled code.
    pub hash: [u8; 32],
    pub bytes: &'a [u8],
}

/// A module found in the component: its bytes, their hash, its module type and that type's hash.
pub(crate) struct Found<'a> {
    pub(crate) bytes: &'a [u8],
    pub(crate) hash: [u8; 32],
    pub(crate) ty: ModuleType,
    pub(crate) key: [u8; 32],
}

/// What one rewrite hoists, how it names imports and what it keeps.
#[derive(Clone, Copy)]
enum Mode {
    /// Hoist every module under its type name; drop custom sections; keep the pinned runtime's
    /// root import.
    Template,
    /// Hoist only the module with SHA-256 `runtime` under the pinned names; keep everything else.
    Pack { runtime: [u8; 32] },
}

impl Mode {
    /// The import name for a module of type `key`, defined at this level (`local`) or forwarded
    /// to a nested component.
    fn name(self, key: &[u8; 32], local: bool) -> String {
        match self {
            Self::Template => name(key),
            Self::Pack { .. } if local => LOCAL.to_owned(),
            Self::Pack { .. } => FORWARD.to_owned(),
        }
    }

    /// Whether an existing instantiation argument name is refused by this rewrite: a template
    /// keeps the arguments that pass the runtime of a packed artifact to its nested components.
    fn refuses(self, name: &str) -> bool {
        match self {
            Self::Template => {
                name.starts_with(PREFIX)
                    || (name.starts_with(RESERVED) && name != LOCAL && name != FORWARD)
            }
            Self::Pack { .. } => name.starts_with(PREFIX) || name.starts_with(RESERVED),
        }
    }
}

/// Rewrite `bytes` into a template that imports every core module. `None` means there is nothing
/// to share, or the shape is not supported or not well formed; the caller then compiles the
/// original component, which reports any real error.
pub fn split(bytes: &[u8]) -> Option<Split<'_>> {
    let mut found = BTreeMap::new();
    let mut runtime = Vec::new();
    let (component, needs) = level(bytes, true, Mode::Template, &mut found, &mut runtime)?;
    if needs.is_empty() && runtime.is_empty() {
        return None;
    }
    let modules = needs
        .into_iter()
        .map(|(key, name)| Hoisted {
            name,
            hash: found[&key].hash,
            bytes: found[&key].bytes,
        })
        .collect();
    Some(Split {
        component,
        modules,
        runtime,
    })
}

/// Hoist the one module whose SHA-256 is `runtime` under the pinned runtime names, keeping every
/// other byte. `Ok(None)` means no module has those bytes.
pub(crate) fn pack(bytes: &[u8], runtime: [u8; 32]) -> Result<Option<Vec<u8>>, ()> {
    let mut found = BTreeMap::new();
    let (component, needs) = level(
        bytes,
        true,
        Mode::Pack { runtime },
        &mut found,
        &mut Vec::new(),
    )
    .ok_or(())?;
    match needs.as_slice() {
        [] => Ok(None),
        [(key, _)] if found[key].hash == runtime => Ok(Some(component)),
        _ => Err(()),
    }
}

/// The template import name of modules whose encoded type has SHA-256 `key`; valid kebab-case.
pub(crate) fn name(key: &[u8; 32]) -> String {
    let mut name = String::with_capacity(PREFIX.len() + 64);
    name.push_str(PREFIX);
    name.push_str(&hex(key));
    name
}

/// Lowercase hexadecimal.
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The top-level sections of one component as `(id, contents)`.
pub(crate) fn sections(bytes: &[u8]) -> Option<Vec<(u8, &[u8])>> {
    if bytes.len() < 8 || !Parser::is_component(bytes) {
        return None;
    }
    let mut reader = BinaryReader::new(&bytes[8..], 8);
    let mut sections = Vec::new();
    while !reader.eof() {
        let id = reader.read_u8().ok()?;
        let size = reader.read_var_u32().ok()?;
        sections.push((id, reader.read_bytes(usize::try_from(size).ok()?).ok()?));
    }
    Some(sections)
}

/// The contents of a core type section that defines exactly `ty`.
pub(crate) fn type_section(ty: &ModuleType) -> Vec<u8> {
    let mut types = CoreTypeSection::new();
    types.ty().module(ty);
    let mut out = wasm_encoder::Component::new();
    out.section(&types);
    let bytes = out.finish();
    let sections = sections(&bytes).expect("an encoded core type section");
    sections[0].1.to_vec()
}

/// Append the module type of `found` and an import of it named `name` at core type index `ty`.
fn import(out: &mut wasm_encoder::Component, found: &Found<'_>, ty: u32, name: &str) {
    let mut types = CoreTypeSection::new();
    types.ty().module(&found.ty);
    out.section(&types);
    let mut imports = ComponentImportSection::new();
    imports.import(name, ComponentTypeRef::Module(ty));
    out.section(&imports);
}

/// Count the core types one core type section defines.
pub(crate) fn core_type_count(data: &[u8]) -> Option<u32> {
    let reader = wasmparser::CoreTypeSectionReader::new(BinaryReader::new(data, 0)).ok()?;
    let mut count = 0u32;
    for ty in reader {
        count = count.checked_add(match ty.ok()? {
            CoreType::Rec(group) => u32::try_from(group.types().len()).ok()?,
            CoreType::Module(_) => 1,
        })?;
    }
    Some(count)
}

/// One rewritten level: its bytes and, in order, the type key of each module it imports with the
/// name it imports it under.
type Level = (Vec<u8>, Vec<([u8; 32], String)>);

/// Rewrite one component level; `None` refuses the whole rewrite.
fn level<'a>(
    bytes: &'a [u8],
    root: bool,
    mode: Mode,
    found: &mut BTreeMap<[u8; 32], Found<'a>>,
    runtime: &mut Vec<&'static str>,
) -> Option<Level> {
    let mut out = wasm_encoder::Component::new();
    // The type keys imported at this level, in order, with their names.
    let mut needs = Vec::new();
    // The module index of each imported type key.
    let mut available = BTreeMap::new();
    // The `(type key, import name)`s each nested component definition imports, by index.
    let mut nested: BTreeMap<u32, Vec<([u8; 32], String)>> = BTreeMap::new();
    let (mut modules, mut core_types, mut components) = (0u32, 0u32, 0u32);
    // Set once an inserted import or type would shift a later item of its index space.
    let (mut shifted, mut typed) = (false, false);
    // Core types inserted at this level so far.
    let mut inserted = 0u32;
    let sections = sections(bytes)?;
    let mut index = 0;
    while index < sections.len() {
        let (id, data) = sections[index];
        index += 1;
        let raw = RawSection { id, data };
        // A packed artifact's runtime reference: re-emitted at the type's output index. The
        // reference check every decode runs guarantees nothing else uses that type.
        if let Mode::Template = mode
            && let Some(slot) = pattern(&sections, index - 1, core_types - inserted).ok()?
        {
            if shifted {
                return None;
            }
            index += 1;
            out.section(&raw);
            let mut imports = ComponentImportSection::new();
            imports.import(slot.name(), ComponentTypeRef::Module(core_types));
            out.section(&imports);
            core_types += 1;
            typed = true;
            modules += 1;
            if root {
                runtime.push(slot.name());
            }
            continue;
        }
        match id {
            MODULE => {
                if shifted {
                    return None;
                }
                if let Mode::Pack { runtime } = mode
                    && <[u8; 32]>::from(Sha256::digest(data)) != runtime
                {
                    out.section(&raw);
                    modules += 1;
                    continue;
                }
                let module = describe(data)?;
                let (key, hash) = (module.key, module.hash);
                // One level cannot import one name twice, and one name never selects two
                // different modules anywhere in the component.
                if available.contains_key(&key) {
                    return None;
                }
                let entry = found.entry(key).or_insert(module);
                if entry.hash != hash {
                    return None;
                }
                let name = mode.name(&key, true);
                import(&mut out, entry, core_types, &name);
                core_types += 1;
                inserted += 1;
                typed = true;
                available.insert(key, modules);
                needs.push((key, name));
                modules += 1;
            }
            CORE_TYPE => {
                if typed {
                    return None;
                }
                core_types = core_types.checked_add(core_type_count(data)?)?;
                out.section(&raw);
            }
            COMPONENT => {
                let (child, child_needs) = level(data, false, mode, found, runtime)?;
                for (key, _) in &child_needs {
                    if available.contains_key(key) {
                        continue;
                    }
                    let name = mode.name(key, false);
                    import(&mut out, &found[key], core_types, &name);
                    core_types += 1;
                    inserted += 1;
                    typed = true;
                    available.insert(*key, modules);
                    modules += 1;
                    shifted = true;
                    needs.push((*key, name));
                }
                out.section(&RawSection {
                    id,
                    data: child.as_slice(),
                });
                if !child_needs.is_empty() {
                    nested.insert(components, child_needs);
                }
                components += 1;
            }
            INSTANCE => {
                let reader =
                    wasmparser::ComponentInstanceSectionReader::new(BinaryReader::new(data, 0))
                        .ok()?;
                let mut section = ComponentInstanceSection::new();
                let mut changed = false;
                for instance in reader {
                    let instance = instance.ok()?;
                    // A rewritten component's type gained imports: it may only be instantiated
                    // here, where the arguments are added.
                    let passes = |kind, index| {
                        kind == ComponentExternalKind::Component && nested.contains_key(&index)
                    };
                    match &instance {
                        ComponentInstance::Instantiate {
                            component_index,
                            args,
                        } => {
                            if args.iter().any(|arg| passes(arg.kind, arg.index)) {
                                return None;
                            }
                            if let Some(keys) = nested.get(component_index) {
                                if args.iter().any(|arg| mode.refuses(arg.name)) {
                                    return None;
                                }
                                let mut forwarded: Vec<(String, ComponentExportKind, u32)> = args
                                    .iter()
                                    .map(|arg| {
                                        (
                                            arg.name.to_owned(),
                                            RoundtripReencoder.component_export_kind(arg.kind),
                                            arg.index,
                                        )
                                    })
                                    .collect();
                                forwarded.extend(keys.iter().map(|(key, name)| {
                                    (name.clone(), ComponentExportKind::Module, available[key])
                                }));
                                section.instantiate(*component_index, forwarded);
                                changed = true;
                                continue;
                            }
                        }
                        ComponentInstance::FromExports(exports) => {
                            if exports
                                .iter()
                                .any(|export| passes(export.kind, export.index))
                            {
                                return None;
                            }
                        }
                    }
                    RoundtripReencoder
                        .parse_component_instance(&mut section, instance)
                        .ok()?;
                }
                if changed {
                    out.section(&section);
                } else {
                    out.section(&raw);
                }
            }
            ALIAS => {
                let reader =
                    wasmparser::ComponentAliasSectionReader::new(BinaryReader::new(data, 0))
                        .ok()?;
                for alias in reader {
                    match alias.ok()? {
                        ComponentAlias::InstanceExport {
                            kind: ComponentExternalKind::Module,
                            ..
                        }
                        | ComponentAlias::Outer {
                            kind: ComponentOuterAliasKind::CoreModule,
                            ..
                        } => {
                            if shifted {
                                return None;
                            }
                            modules += 1;
                        }
                        ComponentAlias::Outer {
                            kind: ComponentOuterAliasKind::CoreType,
                            ..
                        } => {
                            if typed {
                                return None;
                            }
                            core_types += 1;
                        }
                        ComponentAlias::InstanceExport {
                            kind: ComponentExternalKind::Component,
                            ..
                        }
                        | ComponentAlias::Outer {
                            kind: ComponentOuterAliasKind::Component,
                            ..
                        } => components += 1,
                        _ => {}
                    }
                }
                out.section(&raw);
            }
            IMPORT => {
                let reader =
                    wasmparser::ComponentImportSectionReader::new(BinaryReader::new(data, 0))
                        .ok()?;
                for import in reader {
                    let import = import.ok()?;
                    let name = import.name.name;
                    if name.starts_with(PREFIX) || name.starts_with(RESERVED) {
                        return None;
                    }
                    match import.ty {
                        wasmparser::ComponentTypeRef::Module(_) => {
                            // The engine links top-level core modules only for its own imports
                            // and for the pinned runtime, which it provides itself.
                            if root || shifted {
                                return None;
                            }
                            modules += 1;
                        }
                        wasmparser::ComponentTypeRef::Component(_) => components += 1,
                        _ => {}
                    }
                }
                out.section(&raw);
            }
            EXPORT => {
                let reader =
                    wasmparser::ComponentExportSectionReader::new(BinaryReader::new(data, 0))
                        .ok()?;
                for export in reader {
                    let export = export.ok()?;
                    match export.kind {
                        ComponentExternalKind::Module => {
                            if shifted {
                                return None;
                            }
                            modules += 1;
                        }
                        ComponentExternalKind::Component => {
                            if nested.contains_key(&export.index) {
                                return None;
                            }
                            components += 1;
                        }
                        _ => {}
                    }
                }
                out.section(&raw);
            }
            // Identity metadata and names: templates drop them (the original was verified before
            // compilation); a packed artifact keeps every one.
            CUSTOM => {
                if let Mode::Pack { .. } = mode {
                    out.section(&raw);
                }
            }
            CORE_INSTANCE | TYPE | CANONICAL | START => {
                out.section(&raw);
            }
            _ => return None,
        }
    }
    Some((out.finish(), needs))
}

/// Describe one core module as an importable module type, or `None` when it cannot be.
pub(crate) fn describe(bytes: &[u8]) -> Option<Found<'_>> {
    let mut groups = Vec::new();
    let mut imports = Vec::new();
    let mut exports = Vec::new();
    // The type of each function, table, memory, global and tag, imported first.
    let mut funcs = Vec::new();
    let mut tables = Vec::new();
    let mut memories = Vec::new();
    let mut globals = Vec::new();
    let mut tags = Vec::new();
    for payload in Parser::new(0).parse_all(bytes) {
        match payload.ok()? {
            Payload::Version { encoding, .. } if encoding != Encoding::Module => return None,
            Payload::TypeSection(reader) => {
                for group in reader {
                    groups.push(group.ok()?);
                }
            }
            Payload::ImportSection(reader) => {
                for import in reader.into_imports() {
                    let import = import.ok()?;
                    match import.ty {
                        TypeRef::Func(ty) | TypeRef::FuncExact(ty) => funcs.push(ty),
                        TypeRef::Table(ty) => tables.push(ty),
                        TypeRef::Memory(ty) => memories.push(ty),
                        TypeRef::Global(ty) => globals.push(ty),
                        TypeRef::Tag(ty) => tags.push(ty),
                    }
                    imports.push(import);
                }
            }
            Payload::FunctionSection(reader) => {
                for ty in reader {
                    funcs.push(ty.ok()?);
                }
            }
            Payload::TableSection(reader) => {
                for table in reader {
                    tables.push(table.ok()?.ty);
                }
            }
            Payload::MemorySection(reader) => {
                for memory in reader {
                    memories.push(memory.ok()?);
                }
            }
            Payload::GlobalSection(reader) => {
                for global in reader {
                    globals.push(global.ok()?.ty);
                }
            }
            Payload::TagSection(reader) => {
                for tag in reader {
                    tags.push(tag.ok()?);
                }
            }
            Payload::ExportSection(reader) => {
                for export in reader {
                    exports.push(export.ok()?);
                }
            }
            _ => {}
        }
    }
    let mut ty = ModuleType::new();
    for group in groups {
        RoundtripReencoder
            .parse_recursive_type_group(ty.ty(), group)
            .ok()?;
    }
    for import in &imports {
        let entity = RoundtripReencoder.entity_type(import.ty).ok()?;
        ty.import(import.module, import.name, entity);
    }
    for export in &exports {
        let index = usize::try_from(export.index).ok()?;
        let entity = match export.kind {
            ExternalKind::Func => EntityType::Function(*funcs.get(index)?),
            ExternalKind::Table => {
                EntityType::Table(RoundtripReencoder.table_type(*tables.get(index)?).ok()?)
            }
            ExternalKind::Memory => {
                EntityType::Memory(RoundtripReencoder.memory_type(*memories.get(index)?).ok()?)
            }
            ExternalKind::Global => {
                EntityType::Global(RoundtripReencoder.global_type(*globals.get(index)?).ok()?)
            }
            ExternalKind::Tag => {
                EntityType::Tag(RoundtripReencoder.tag_type(*tags.get(index)?).ok()?)
            }
            ExternalKind::FuncExact => return None,
        };
        ty.export(export.name, entity);
    }
    let mut encoded = Vec::new();
    ty.encode(&mut encoded);
    Some(Found {
        bytes,
        hash: Sha256::digest(bytes).into(),
        key: Sha256::digest(&encoded).into(),
        ty,
    })
}
