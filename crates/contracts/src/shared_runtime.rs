//! The pinned QuickJS runtime core shared by every source Orb.
//!
//! Every source Orb used to embed the same ~2 MB runtime core module next to its own snapshot.
//! A production source artifact instead imports that one module: it is a standard component in
//! which the runtime's module definition is replaced, at its own index, by
//!
//! ```text
//! (core type $t (module …the pinned module type…))
//! (import "tembra-runtime-core-h<sha256>" (core module (type $t)))
//! ```
//!
//! and each enclosing component imports it the same way under `tembra-runtime-forward-h<sha256>`
//! and passes it to the nested component's instantiation. Only the snapshot, bindings and
//! adapters stay embedded. The names carry the SHA-256 of the runtime core bytes, so an artifact
//! built against another runtime names a module this host does not have.
//!
//! Nothing is trusted from the artifact: [`references`] checks every reference's exact names,
//! exact module type (compared byte for byte with the pinned type), position and forwarding
//! before any guest code is compiled. Native engines link their own compiled copy of the pinned
//! module; [`SharedRuntime::expand`] restores the inline component for hosts (the browser glue
//! generator) that cannot link core modules. Compiled components may import no core module.
use crate::Error;
use crate::shared::{
    ALIAS, CANONICAL, COMPONENT, CORE_INSTANCE, CORE_TYPE, CUSTOM, EXPORT, IMPORT, INSTANCE,
    MODULE, START, TYPE, core_type_count, describe, hex, sections, type_section,
};
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::collections::BTreeMap;
use wasm_encoder::reencode::{ReencodeComponent, RoundtripReencoder};
use wasm_encoder::{ComponentInstanceSection, RawSection};
use wasmparser::{
    BinaryReader, ComponentAlias, ComponentExternalKind, ComponentInstance, ComponentOuterAliasKind,
};

/// The SHA-256 of the pinned runtime core (`runtime/dquickts.wasm`), as a literal.
macro_rules! digest {
    () => {
        "8fb136ccb6adace2596069f82abe8914b268688d32a12b61505d4527a9f93d3e"
    };
}

/// SHA-256 of the canonical core type section that defines exactly the pinned module type.
const TYPE_DIGEST: &str = "b7978d25210e301951c440f65ff009a8e92a924a8d9521a41b083e02c1443a0a";

/// Prefix of every shared-runtime import name; a component never uses it otherwise.
pub(crate) const RESERVED: &str = "tembra-runtime-";
/// The import that stands for the runtime's own module definition.
pub(crate) const LOCAL: &str = concat!("tembra-runtime-core-h", digest!());
/// The import that only forwards the runtime to a nested component.
pub(crate) const FORWARD: &str = concat!("tembra-runtime-forward-h", digest!());

/// Bound on component nesting while checking references.
const MAX_DEPTH: usize = 64;

/// The checked bytes of the pinned runtime core.
pub struct SharedRuntime {
    bytes: Cow<'static, [u8]>,
    hash: [u8; 32],
}

impl SharedRuntime {
    /// SHA-256 of the pinned runtime core, lowercase hexadecimal.
    pub const SHA256: &'static str = digest!();

    /// Accept exactly the pinned runtime core bytes.
    pub fn new(bytes: impl Into<Cow<'static, [u8]>>) -> Result<Self, Error> {
        let bytes = bytes.into();
        let hash: [u8; 32] = Sha256::digest(&bytes).into();
        let actual = hex(&hash);
        if actual != Self::SHA256 {
            return Err(Error(format!(
                "runtime core {actual} is not the pinned source runtime {}",
                Self::SHA256
            )));
        }
        // Validate immutable interpreter code once, rather than again for every expansion.
        validate(&bytes, "pinned source runtime")?;
        let runtime = Self { bytes, hash };
        if hex(&Sha256::digest(runtime.type_section()?)) != TYPE_DIGEST {
            return Err(Error(
                "the pinned source runtime does not have its pinned module type".into(),
            ));
        }
        Ok(runtime)
    }

    /// The runtime core packaged with this host, checked once.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn pinned() -> &'static SharedRuntime {
        static PINNED: std::sync::LazyLock<SharedRuntime> = std::sync::LazyLock::new(|| {
            SharedRuntime::new(&include_bytes!("../runtime/dquickts.wasm")[..])
                .expect("the packaged runtime core is the pinned one")
        });
        &PINNED
    }

    /// The runtime core module.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The SHA-256 of [`Self::bytes`].
    pub fn hash(&self) -> [u8; 32] {
        self.hash
    }

    /// The core type section contents defining exactly this module's type.
    fn type_section(&self) -> Result<Vec<u8>, Error> {
        let found = describe(&self.bytes)
            .ok_or_else(|| Error("the runtime core has no importable module type".into()))?;
        Ok(type_section(&found.ty))
    }

    /// Replace the embedded runtime core of a source component by the pinned import, keeping
    /// every other byte. Fails when the component does not embed exactly these runtime bytes or
    /// has a shape the rewrite does not support; the result is checked to expand back to
    /// `component` exactly.
    pub fn pack(&self, component: &[u8]) -> Result<Vec<u8>, Error> {
        let thin = match crate::shared::pack(component, self.hash) {
            Ok(Some(thin)) => thin,
            Ok(None) => {
                return Err(Error(format!(
                    "the component does not embed the pinned source runtime core {}; the runtime \
                     dylib does not match this host",
                    Self::SHA256
                )));
            }
            Err(()) => {
                return Err(Error(
                    "the component's runtime core cannot be shared: unsupported component shape"
                        .into(),
                ));
            }
        };
        if !references(&thin)? || self.expand(&thin)? != component {
            return Err(Error(
                "the shared runtime packaging does not reproduce the component".into(),
            ));
        }
        Ok(thin)
    }

    /// Restore the runtime core inline: each local reference becomes the module itself and each
    /// forwarding import and argument is removed. Validate the thin component and substitute the
    /// already checked module with exactly its declared type. Only for hosts that cannot link
    /// core modules; native engines link the pinned module instead.
    pub fn expand(&self, thin: &[u8]) -> Result<Vec<u8>, Error> {
        validate(thin, "thin source component")?;
        let (expanded, slot) = walk(thin, &[], Some(&self.bytes), &mut false)?;
        if slot.is_none() {
            return Err(Error(
                "the component does not reference the shared source runtime".into(),
            ));
        }
        Ok(expanded.expect("expansion output"))
    }
}

/// Check executable bytes before transformation; reference checks preserve the index spaces.
fn validate(bytes: &[u8], label: &str) -> Result<(), Error> {
    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(bytes)
        .map_err(|error| Error(format!("{label} is invalid: {error}")))?;
    Ok(())
}

/// Whether a top-level import name refers to the pinned runtime core.
pub fn shared_runtime_import(name: &str) -> bool {
    name == LOCAL || name == FORWARD
}

/// Check every shared-runtime reference of a component without the runtime bytes and report
/// whether the root imports the runtime. Refuses any other root core-module import.
pub(crate) fn references(bytes: &[u8]) -> Result<bool, Error> {
    Ok(walk(bytes, &[], None, &mut false)?.1.is_some())
}

/// How one level imports the runtime.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Slot {
    /// In place of its own module definition.
    Local,
    /// Only to pass it to nested components.
    Forward,
}

impl Slot {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Local => LOCAL,
            Self::Forward => FORWARD,
        }
    }
}

/// Whether `sections[index]` starts the exact runtime reference: the pinned core type section,
/// defining core type `ty`, immediately followed by one pinned module import of that type.
pub(crate) fn pattern(
    sections: &[(u8, &[u8])],
    index: usize,
    ty: u32,
) -> Result<Option<Slot>, Error> {
    match (sections.get(index), sections.get(index + 1)) {
        (Some(&(CORE_TYPE, types)), Some(&(IMPORT, imports)))
            if hex(&Sha256::digest(types)) == TYPE_DIGEST =>
        {
            slot(imports, ty)
        }
        _ => Ok(None),
    }
}

fn unsupported(detail: &str) -> Error {
    Error(format!("unsupported shared runtime reference: {detail}"))
}

fn malformed() -> Error {
    Error("malformed component".into())
}

/// The runtime slot of one import section that holds exactly one pinned module import of core
/// type `ty`, or `None` when the section is anything else.
fn slot(data: &[u8], ty: u32) -> Result<Option<Slot>, Error> {
    let reader = wasmparser::ComponentImportSectionReader::new(BinaryReader::new(data, 0))
        .map_err(|_| malformed())?;
    if reader.count() != 1 {
        return Ok(None);
    }
    let import = reader
        .into_iter()
        .next()
        .and_then(Result::ok)
        .ok_or_else(malformed)?;
    let slot = match import.name.name {
        LOCAL => Slot::Local,
        FORWARD => Slot::Forward,
        _ => return Ok(None),
    };
    if !matches!(import.ty, wasmparser::ComponentTypeRef::Module(index) if index == ty) {
        return Err(unsupported(
            "the runtime import does not use its pinned type",
        ));
    }
    Ok(Some(slot))
}

/// Refuse a reserved import name that is not part of an exact runtime reference.
fn reserved(name: &str) -> Result<(), Error> {
    if !name.starts_with(RESERVED) {
        return Ok(());
    }
    if shared_runtime_import(name) {
        return Err(unsupported("a runtime import without its pinned type"));
    }
    let required = name
        .rsplit_once("-h")
        .map_or(name, |(_, digest)| digest)
        .to_owned();
    Err(Error(format!(
        "the source Orb requires runtime core {required}; this host provides {}",
        SharedRuntime::SHA256
    )))
}

/// Copy one section unchanged when writing an expansion.
fn emit(out: &mut Option<wasm_encoder::Component>, id: u8, data: &[u8]) {
    if let Some(out) = out {
        out.section(&RawSection { id, data });
    }
}

/// What a nested component may see of one enclosing level's runtime reference.
#[derive(Clone, Copy, Default)]
struct Scope {
    /// The module index of a forwarding import, which expansion removes.
    forward: Option<u32>,
    /// The core type index of the runtime's type, which expansion removes and engine templates
    /// renumber; nothing but the runtime import may use it.
    ty: Option<u32>,
}

/// The scope an outer alias with `count` selects, if it is one of `scopes` (`scopes[0]` is one
/// step out).
fn outer_scope<T: Copy>(scopes: &[T], count: u32) -> Option<T> {
    let step = usize::try_from(count).ok()?.checked_sub(1)?;
    scopes.get(step).copied()
}

/// Refuse a type declaration that aliases a runtime type; `scopes[0]` is the scope enclosing
/// `ty`, holding its runtime type index if any.
fn check_type(ty: &wasmparser::ComponentType<'_>, scopes: &[Option<u32>]) -> Result<(), Error> {
    let check_alias = |alias: &ComponentAlias<'_>| match alias {
        ComponentAlias::Outer {
            kind: ComponentOuterAliasKind::CoreType,
            count,
            index,
        } if outer_scope(scopes, *count) == Some(Some(*index)) => {
            Err(unsupported("a type aliases the runtime's type"))
        }
        _ => Ok(()),
    };
    let nested = |ty: &wasmparser::ComponentType<'_>| {
        let mut inner = Vec::with_capacity(scopes.len() + 1);
        inner.push(None);
        inner.extend_from_slice(scopes);
        check_type(ty, &inner)
    };
    match ty {
        wasmparser::ComponentType::Component(declarations) => {
            for declaration in declarations.iter() {
                match declaration {
                    wasmparser::ComponentTypeDeclaration::Alias(alias) => check_alias(alias)?,
                    wasmparser::ComponentTypeDeclaration::Type(ty) => nested(ty)?,
                    _ => {}
                }
            }
        }
        wasmparser::ComponentType::Instance(declarations) => {
            for declaration in declarations.iter() {
                match declaration {
                    wasmparser::InstanceTypeDeclaration::Alias(alias) => check_alias(alias)?,
                    wasmparser::InstanceTypeDeclaration::Type(ty) => nested(ty)?,
                    _ => {}
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// Check one component level and, given the runtime bytes, write it with the runtime inline.
///
/// `outer[k]` describes the runtime reference of the level `k + 1` steps up. Expansion removes
/// each runtime type and each forwarding import, so nothing but the reference itself may use
/// them; this keeps an expansion and an engine template equivalent to the checked component.
fn walk(
    bytes: &[u8],
    outer: &[Scope],
    runtime: Option<&[u8]>,
    defined: &mut bool,
) -> Result<(Option<Vec<u8>>, Option<Slot>), Error> {
    if outer.len() > MAX_DEPTH {
        return Err(unsupported("components nest too deeply"));
    }
    let sections = sections(bytes).ok_or_else(malformed)?;
    let mut out = runtime.map(|_| wasm_encoder::Component::new());
    let (mut modules, mut core_types, mut components) = (0u32, 0u32, 0u32);
    // This level's runtime import, its module index and its core type index.
    let mut own: Option<(Slot, u32, u32)> = None;
    // Nested component definitions that import the runtime, by component index.
    let mut nested: BTreeMap<u32, Slot> = BTreeMap::new();
    let scope = |own: Option<(Slot, u32, u32)>| Scope {
        forward: own.and_then(|(slot, index, _)| (slot == Slot::Forward).then_some(index)),
        ty: own.map(|(_, _, ty)| ty),
    };
    let mut index = 0;
    while index < sections.len() {
        let (id, data) = sections[index];
        index += 1;
        // The exact reference: the pinned type section immediately followed by its import.
        if let Some(slot) = pattern(&sections, index - 1, core_types)? {
            index += 1;
            if own.is_some() {
                return Err(unsupported("two runtime imports in one component"));
            }
            // One definition bounds expansion to the input plus one immutable core, even if
            // the component contains many nested definitions or instantiates one repeatedly.
            if slot == Slot::Local && std::mem::replace(defined, true) {
                return Err(unsupported(
                    "a source Orb may define the shared runtime only once",
                ));
            }
            if slot == Slot::Local
                && let Some(out) = &mut out
            {
                out.section(&RawSection {
                    id: MODULE,
                    data: runtime.expect("runtime bytes"),
                });
            }
            own = Some((slot, modules, core_types));
            core_types += 1;
            modules += 1;
            continue;
        }
        // Expansion removes this level's runtime type; with a forwarding import, also its module.
        let current = scope(own);
        let sealed_types = own.is_some();
        let sealed_modules = current.forward.is_some();
        match id {
            CORE_TYPE => {
                if sealed_types {
                    return Err(unsupported("a core type follows the runtime import"));
                }
                core_types = core_types
                    .checked_add(core_type_count(data).ok_or_else(malformed)?)
                    .ok_or_else(malformed)?;
                emit(&mut out, id, data);
            }
            MODULE => {
                if sealed_modules {
                    return Err(unsupported("a core module follows the forwarded runtime"));
                }
                modules += 1;
                emit(&mut out, id, data);
            }
            COMPONENT => {
                let mut scopes = Vec::with_capacity(outer.len() + 1);
                scopes.push(current);
                scopes.extend_from_slice(outer);
                let (child, slot) = walk(data, &scopes, runtime, defined)?;
                if let Some(slot) = slot {
                    nested.insert(components, slot);
                }
                components += 1;
                emit(&mut out, id, child.as_deref().unwrap_or(data));
            }
            INSTANCE => {
                let reader =
                    wasmparser::ComponentInstanceSectionReader::new(BinaryReader::new(data, 0))
                        .map_err(|_| malformed())?;
                let mut section = ComponentInstanceSection::new();
                let mut changed = false;
                for instance in reader {
                    let instance = instance.map_err(|_| malformed())?;
                    // A component that imports the runtime changes type on expansion: it may only
                    // be instantiated here.
                    let passes = |kind, index| {
                        (kind == ComponentExternalKind::Component && nested.contains_key(&index))
                            || (kind == ComponentExternalKind::Module
                                && current.forward == Some(index))
                    };
                    match &instance {
                        ComponentInstance::Instantiate {
                            component_index,
                            args,
                        } => {
                            let expected = nested.get(component_index).map(|slot| slot.name());
                            let mut supplied = 0;
                            let mut kept = Vec::with_capacity(args.len());
                            for arg in args.iter() {
                                if !arg.name.starts_with(RESERVED) {
                                    if passes(arg.kind, arg.index) {
                                        return Err(unsupported(
                                            "the runtime or a component importing it is passed on",
                                        ));
                                    }
                                    kept.push((
                                        arg.name,
                                        RoundtripReencoder.component_export_kind(arg.kind),
                                        arg.index,
                                    ));
                                    continue;
                                }
                                // Only this level's own runtime import satisfies a nested one.
                                if Some(arg.name) != expected
                                    || arg.kind != ComponentExternalKind::Module
                                    || own.map(|(_, index, _)| index) != Some(arg.index)
                                {
                                    return Err(unsupported(
                                        "a runtime argument is not this component's own runtime",
                                    ));
                                }
                                supplied += 1;
                            }
                            if expected.is_some() && supplied != 1 {
                                return Err(unsupported(
                                    "a nested runtime import is not supplied exactly once",
                                ));
                            }
                            if supplied != 0 {
                                section.instantiate(*component_index, kept);
                                changed = true;
                                continue;
                            }
                        }
                        ComponentInstance::FromExports(exports) => {
                            if exports
                                .iter()
                                .any(|export| passes(export.kind, export.index))
                            {
                                return Err(unsupported(
                                    "the runtime or a component importing it is exported",
                                ));
                            }
                        }
                    }
                    if out.is_some() {
                        RoundtripReencoder
                            .parse_component_instance(&mut section, instance)
                            .map_err(|_| malformed())?;
                    }
                }
                match &mut out {
                    Some(out) if changed => {
                        out.section(&section);
                    }
                    _ => emit(&mut out, id, data),
                }
            }
            CORE_INSTANCE => {
                let reader = wasmparser::InstanceSectionReader::new(BinaryReader::new(data, 0))
                    .map_err(|_| malformed())?;
                for instance in reader {
                    if let wasmparser::Instance::Instantiate { module_index, .. } =
                        instance.map_err(|_| malformed())?
                        && current.forward == Some(module_index)
                    {
                        return Err(unsupported("the forwarded runtime is instantiated"));
                    }
                }
                emit(&mut out, id, data);
            }
            ALIAS => {
                let reader =
                    wasmparser::ComponentAliasSectionReader::new(BinaryReader::new(data, 0))
                        .map_err(|_| malformed())?;
                for alias in reader {
                    match alias.map_err(|_| malformed())? {
                        ComponentAlias::Outer {
                            kind: ComponentOuterAliasKind::CoreModule,
                            count,
                            index,
                        } => {
                            let selected =
                                outer_scope(outer, count).and_then(|scope| scope.forward);
                            if sealed_modules || selected == Some(index) {
                                return Err(unsupported("a module alias of the runtime"));
                            }
                            modules += 1;
                        }
                        ComponentAlias::InstanceExport {
                            kind: ComponentExternalKind::Module,
                            ..
                        } => {
                            if sealed_modules {
                                return Err(unsupported("a module alias of the runtime"));
                            }
                            modules += 1;
                        }
                        ComponentAlias::Outer {
                            kind: ComponentOuterAliasKind::CoreType,
                            count,
                            index,
                        } => {
                            if sealed_types {
                                return Err(unsupported("a core type follows the runtime import"));
                            }
                            if outer_scope(outer, count).and_then(|scope| scope.ty) == Some(index) {
                                return Err(unsupported("a type aliases the runtime's type"));
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
                emit(&mut out, id, data);
            }
            IMPORT => {
                let reader =
                    wasmparser::ComponentImportSectionReader::new(BinaryReader::new(data, 0))
                        .map_err(|_| malformed())?;
                for import in reader {
                    let import = import.map_err(|_| malformed())?;
                    reserved(import.name.name)?;
                    match import.ty {
                        wasmparser::ComponentTypeRef::Module(ty) => {
                            if current.ty == Some(ty) {
                                return Err(unsupported("another import uses the runtime's type"));
                            }
                            if outer.is_empty() {
                                return Err(Error(format!(
                                    "the component imports core module `{}`; only source \
                                     components may import the pinned source runtime",
                                    import.name.name
                                )));
                            }
                            if sealed_modules {
                                return Err(unsupported(
                                    "a core module follows the forwarded runtime",
                                ));
                            }
                            modules += 1;
                        }
                        wasmparser::ComponentTypeRef::Component(_) => components += 1,
                        _ => {}
                    }
                }
                emit(&mut out, id, data);
            }
            EXPORT => {
                let reader =
                    wasmparser::ComponentExportSectionReader::new(BinaryReader::new(data, 0))
                        .map_err(|_| malformed())?;
                for export in reader {
                    let export = export.map_err(|_| malformed())?;
                    if let Some(wasmparser::ComponentTypeRef::Module(ty)) = export.ty
                        && current.ty == Some(ty)
                    {
                        return Err(unsupported("an export uses the runtime's type"));
                    }
                    match export.kind {
                        ComponentExternalKind::Module => {
                            if own.map(|(_, index, _)| index) == Some(export.index) {
                                return Err(unsupported("the runtime is exported"));
                            }
                            if sealed_modules {
                                return Err(unsupported(
                                    "a core module follows the forwarded runtime",
                                ));
                            }
                            modules += 1;
                        }
                        ComponentExternalKind::Component => {
                            if nested.contains_key(&export.index) {
                                return Err(unsupported(
                                    "a component importing the runtime is exported",
                                ));
                            }
                            components += 1;
                        }
                        _ => {}
                    }
                }
                emit(&mut out, id, data);
            }
            TYPE => {
                let reader =
                    wasmparser::ComponentTypeSectionReader::new(BinaryReader::new(data, 0))
                        .map_err(|_| malformed())?;
                let mut scopes = Vec::with_capacity(outer.len() + 1);
                scopes.push(current.ty);
                scopes.extend(outer.iter().map(|scope| scope.ty));
                for ty in reader {
                    check_type(&ty.map_err(|_| malformed())?, &scopes)?;
                }
                emit(&mut out, id, data);
            }
            CUSTOM | CANONICAL | START => emit(&mut out, id, data),
            _ => return Err(malformed()),
        }
    }
    // A forwarding import must be what some nested component imports.
    if scope(own).forward.is_some() && nested.is_empty() {
        return Err(unsupported("a forwarded runtime no component imports"));
    }
    Ok((out.map(|out| out.finish()), own.map(|(slot, _, _)| slot)))
}
