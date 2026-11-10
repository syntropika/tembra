//! The compact, parser-free contract carried by every Orb artifact.
//!
//! Authoring tools lower a resolved WIT world into this typed model once. Runtimes decode it,
//! re-check every bound and reference and never rely on hashes or flags supplied by the artifact.
use crate::{Error, codec};
use std::{collections::BTreeMap, sync::Arc};

/// Maximum type definitions retained for one selected world.
pub(crate) const MAX_TYPES: usize = 4096;
/// Maximum imports or exports on one side of a world.
pub(crate) const MAX_ITEMS: usize = 256;
/// Maximum functions, fields, cases, flags or parameters in one declaration.
pub(crate) const MAX_MEMBERS: usize = 1024;
/// Maximum UTF-8 bytes in any WIT name or interface identity.
pub(crate) const MAX_NAME: usize = 512;
/// Maximum structural type nesting, shared with value validation.
pub(crate) const MAX_DEPTH: usize = 64;
/// Maximum encoded contract bytes accepted from an artifact.
pub(crate) const MAX_BYTES: usize = 4 << 20;
/// Maximum size of the selected world's signatures with every shared type reference expanded.
///
/// Profile checks, interface digests and component verification walk signatures structurally,
/// so a compact DAG of shared definitions could otherwise expand exponentially. One unit is one
/// type node or one byte of a type, member or function name; real worlds use a few thousand.
pub(crate) const MAX_EXPANSION: u64 = 1 << 24;

/// A WIT value type: a primitive or a reference to an earlier type definition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Ty {
    Bool,
    U8,
    U16,
    U32,
    U64,
    S8,
    S16,
    S32,
    S64,
    F32,
    F64,
    Char,
    String,
    ErrorContext,
    Ref(u32),
}

/// A named record field or function parameter.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Field {
    pub name: String,
    pub ty: Ty,
}

/// A variant case with an optional payload.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Case {
    pub name: String,
    pub ty: Option<Ty>,
}

/// The declaring WIT name of a resource, before any `use` alias in another interface.
///
/// Anonymous interfaces have no package identity. Their types can still be inspected and
/// compared within one component, but cannot name a resource contract between Orbs.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ResourceIdentity {
    pub interface: Option<String>,
    pub name: Option<String>,
}

/// The structure of one type definition. Handles refer to the defining resource's index.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Alias(Ty),
    Record(Vec<Field>),
    Tuple(Vec<Ty>),
    Variant(Vec<Case>),
    Enum(Vec<String>),
    Flags(Vec<String>),
    Option(Ty),
    Result { ok: Option<Ty>, err: Option<Ty> },
    List(Ty),
    FixedList(Ty, u32),
    Map(Ty, Ty),
    Resource(ResourceIdentity),
    Own(u32),
    Borrow(u32),
    Future(Option<Ty>),
    Stream(Option<Ty>),
}

impl Kind {
    /// Use the official WIT spelling in diagnostics.
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Alias(_) => "type",
            Self::Record(_) => "record",
            Self::Tuple(_) => "tuple",
            Self::Variant(_) => "variant",
            Self::Enum(_) => "enum",
            Self::Flags(_) => "flags",
            Self::Option(_) => "option",
            Self::Result { .. } => "result",
            Self::List(_) => "list",
            Self::FixedList(..) => "fixed-length list",
            Self::Map(..) => "map",
            Self::Resource(_) => "resource",
            Self::Own(_) => "own",
            Self::Borrow(_) => "borrow",
            Self::Future(_) => "future",
            Self::Stream(_) => "stream",
        }
    }

    /// Visit every value type directly referenced by this definition.
    pub(crate) fn children(&self, mut visit: impl FnMut(Ty)) {
        match self {
            Self::Alias(ty) | Self::Option(ty) | Self::List(ty) | Self::FixedList(ty, _) => {
                visit(*ty)
            }
            Self::Map(key, value) => {
                visit(*key);
                visit(*value);
            }
            Self::Record(fields) => fields.iter().for_each(|field| visit(field.ty)),
            Self::Tuple(types) => types.iter().copied().for_each(visit),
            Self::Variant(cases) => cases.iter().filter_map(|case| case.ty).for_each(visit),
            Self::Result { ok, err } => [*ok, *err].into_iter().flatten().for_each(visit),
            Self::Future(ty) | Self::Stream(ty) => ty.iter().copied().for_each(visit),
            Self::Own(id) | Self::Borrow(id) => visit(Ty::Ref(*id)),
            Self::Enum(_) | Self::Flags(_) | Self::Resource(_) => {}
        }
    }
}

/// Function classes from the component model; resource-scoped forms name their resource.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FunctionKind {
    Freestanding,
    AsyncFreestanding,
    Getter,
    Setter,
    Method(u32),
    AsyncMethod(u32),
    MethodGetter(u32),
    MethodSetter(u32),
    Static(u32),
    AsyncStatic(u32),
    StaticGetter(u32),
    StaticSetter(u32),
    Constructor(u32),
}

impl FunctionKind {
    /// Return the resource definition a scoped function belongs to.
    pub(crate) fn resource(self) -> Option<u32> {
        match self {
            Self::Freestanding | Self::AsyncFreestanding | Self::Getter | Self::Setter => None,
            Self::Method(id)
            | Self::AsyncMethod(id)
            | Self::MethodGetter(id)
            | Self::MethodSetter(id)
            | Self::Static(id)
            | Self::AsyncStatic(id)
            | Self::StaticGetter(id)
            | Self::StaticSetter(id)
            | Self::Constructor(id) => Some(id),
        }
    }

    /// Plain value services only accept freestanding functions.
    pub(crate) fn freestanding(self) -> bool {
        matches!(self, Self::Freestanding | Self::AsyncFreestanding)
    }

    /// Use the WIT kebab-case spelling in inspection output.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Freestanding => "freestanding",
            Self::AsyncFreestanding => "async-freestanding",
            Self::Getter => "getter",
            Self::Setter => "setter",
            Self::Method(_) => "method",
            Self::AsyncMethod(_) => "async-method",
            Self::MethodGetter(_) => "method-getter",
            Self::MethodSetter(_) => "method-setter",
            Self::Static(_) => "static",
            Self::AsyncStatic(_) => "async-static",
            Self::StaticGetter(_) => "static-getter",
            Self::StaticSetter(_) => "static-setter",
            Self::Constructor(_) => "constructor",
        }
    }
}

/// A complete function signature with its original WIT name.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Function {
    pub name: String,
    pub kind: FunctionKind,
    pub params: Vec<Field>,
    pub result: Option<Ty>,
}

/// An imported or exported interface.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Interface {
    /// Versioned package identity such as `ns:pkg/name@1.0.0`; absent for inline interfaces.
    pub contract: Option<String>,
    /// Declared interface name used for the JavaScript service alias.
    pub name: Option<String>,
    /// Resource types exported by this interface, keyed by their local WIT names. A `use`
    /// alias keeps its own local name while its index resolves to the original definition.
    pub resources: BTreeMap<String, u32>,
    /// Whether the world imports this interface itself (directly, through `include`, or under a
    /// named alias), rather than only because another item uses its types. Always true for
    /// exports. Contracts decoded without provenance mark every import explicit.
    pub explicit: bool,
    pub functions: Vec<Function>,
}

impl Interface {
    /// Whether importing this interface carries callable authority, so it needs a provider and
    /// a grant. A type-only dependency carries none even though elaboration keeps its functions
    /// for type identity. A dependency that owns or aliases a resource type stays fully callable
    /// and grant-bound, exactly like an explicit import of it. That includes constructors and
    /// static functions that create handles without one (for example `tcp-socket.create` in
    /// `wasi:sockets/types` or the `wasi:http/types` constructors). Its grant, the handle checks
    /// and each operation's own grant limits (connect/bind lists, mounts, origins) still bound
    /// what those calls can reach.
    pub fn callable(&self) -> bool {
        (self.explicit || !self.resources.is_empty())
            && (!self.resources.is_empty() || !self.functions.is_empty())
    }
}

/// One world import or export.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Item {
    Interface(Interface),
    Function(Function),
    Type(Ty),
}

/// A world item keyed by its world name (`name_world_key`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Entry {
    pub key: String,
    pub item: Item,
}

/// The selected world with only the type definitions its items reach.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Model {
    pub world: String,
    pub package: Option<String>,
    pub types: Vec<Kind>,
    pub imports: Vec<Entry>,
    pub exports: Vec<Entry>,
}

/// A bound function: side, world item and function index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Slot {
    pub exports: bool,
    pub entry: u32,
    pub function: u32,
}

/// Checked runtime contract shared cheaply by every installation of one artifact.
#[derive(Clone, Debug)]
pub struct PreparedContract(Arc<Prepared>);

#[derive(Debug)]
pub(crate) struct Prepared {
    pub model: Model,
    pub slots: Vec<Slot>,
    /// JavaScript spellings of record fields and flags, indexed by type and member order.
    ///
    /// Derived once from `model`, so value checks do not allocate names. Other kinds stay empty.
    pub spellings: Vec<Box<[Box<str>]>>,
    /// Whether each type definition reaches a `u64` or `s64`, indexed by type.
    ///
    /// Derived once from `model`, so wire conversion skips every value without 64-bit integers.
    pub wide: Vec<bool>,
}

impl PartialEq for PreparedContract {
    /// Compare the declared contract, independent of how it was produced.
    fn eq(&self, other: &Self) -> bool {
        self.0.model == other.0.model
    }
}

impl Eq for PreparedContract {}

impl PreparedContract {
    /// Check a model completely before it can answer any query.
    pub fn new(model: Model) -> Result<Self, Error> {
        model.check()?;
        let mut slots = Vec::new();
        for exports in [true, false] {
            let side = if exports {
                &model.exports
            } else {
                &model.imports
            };
            for (entry, item) in side.iter().enumerate() {
                if let Item::Interface(interface) = &item.item {
                    for function in 0..interface.functions.len() {
                        slots.push(Slot {
                            exports,
                            entry: entry as u32,
                            function: function as u32,
                        });
                    }
                }
            }
        }
        let spellings = model.types.iter().map(spellings).collect();
        // `check` ordered references backwards, so one forward pass sees every child first.
        let mut wide = Vec::with_capacity(model.types.len());
        for kind in &model.types {
            let mut reaches = false;
            kind.children(|ty| {
                reaches |= match ty {
                    Ty::U64 | Ty::S64 => true,
                    Ty::Ref(id) => wide[id as usize],
                    _ => false,
                };
            });
            wide.push(reaches);
        }
        Ok(Self(Arc::new(Prepared {
            model,
            slots,
            spellings,
            wide,
        })))
    }

    /// Decode and fully re-validate an untrusted compact contract.
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > MAX_BYTES {
            return Err(Error("prepared contract exceeds its size limit".into()));
        }
        let mut reader = codec::Reader::new(bytes);
        let model = codec::model(&mut reader)?;
        reader.finish()?;
        Self::new(model)
    }

    /// Encode the canonical compact form embedded in artifacts.
    pub fn encode(&self) -> Vec<u8> {
        let mut writer = codec::Writer::default();
        codec::write_model(&mut writer, &self.0.model);
        writer.finish()
    }

    /// Read the typed model.
    pub fn model(&self) -> &Model {
        &self.0.model
    }

    /// Selected world name.
    pub fn world(&self) -> &str {
        &self.0.model.world
    }

    pub(crate) fn slots(&self) -> &[Slot] {
        &self.0.slots
    }

    /// Resolve a type reference that `Model::check` has already bounded.
    pub(crate) fn kind(&self, id: u32) -> &Kind {
        &self.0.model.types[id as usize]
    }

    /// Resolve aliases to a resource's declaring WIT identity. Arena indexes are local to this
    /// prepared contract and must never identify a resource type in another Orb's contract.
    pub fn resource_identity(&self, mut ty: Ty) -> Option<&ResourceIdentity> {
        while let Ty::Ref(id) = ty {
            match self.0.model.types.get(id as usize)? {
                Kind::Alias(next) => ty = *next,
                Kind::Resource(identity) => return Some(identity),
                _ => return None,
            }
        }
        None
    }

    /// Read the cached JavaScript spellings of a record's fields or a flags type's flags.
    pub(crate) fn spellings(&self, id: u32) -> &[Box<str>] {
        &self.0.spellings[id as usize]
    }

    /// Whether a value of `ty` can contain a `u64` or `s64`.
    pub(crate) fn wide(&self, ty: Ty) -> bool {
        match ty {
            Ty::U64 | Ty::S64 => true,
            Ty::Ref(id) => self.0.wide[id as usize],
            _ => false,
        }
    }

    /// Follow aliases to the defining kind.
    pub(crate) fn resolved(&self, mut ty: Ty) -> Ty {
        while let Ty::Ref(id) = ty {
            match self.kind(id) {
                Kind::Alias(next) => ty = *next,
                _ => break,
            }
        }
        ty
    }
}

impl Model {
    /// Enforce limits, acyclic references, resource targets, unique names and expansion size.
    fn check(&self) -> Result<(), Error> {
        name(&self.world, false)?;
        if let Some(package) = &self.package {
            identity(package)?;
        }
        if self.types.len() > MAX_TYPES {
            return Err(Error("prepared contract has too many types".into()));
        }
        // Backward-only references make definition order topological, so nesting depth and
        // expanded size are computed in one pass instead of by walking the shared graph.
        let mut depths = Vec::with_capacity(self.types.len());
        let mut sizes = Vec::with_capacity(self.types.len());
        let mut nominal = std::collections::BTreeSet::new();
        for (index, kind) in self.types.iter().enumerate() {
            if let Kind::Resource(ResourceIdentity {
                interface: Some(interface),
                name: Some(name),
            }) = kind
                && !nominal.insert((interface, name))
            {
                return Err(Error("duplicate nominal resource definition".into()));
            }
            let mut depth = 0;
            let mut size = 1 + member_names(kind);
            let mut failure = None;
            kind.children(|ty| {
                size = size.saturating_add(match ty {
                    // References may only point backwards, which makes cycles unrepresentable.
                    Ty::Ref(id) => match depths.get(id as usize) {
                        Some(child) if (id as usize) < index => {
                            depth = depth.max(child + 1);
                            sizes[id as usize]
                        }
                        _ => {
                            failure = Some("prepared type reference is out of order");
                            0
                        }
                    },
                    _ => 1,
                });
            });
            if let Some(failure) = failure {
                return Err(Error(failure.into()));
            }
            if depth > MAX_DEPTH {
                return Err(Error("prepared type nesting exceeds its limit".into()));
            }
            depths.push(depth);
            sizes.push(expansion(size)?);
            self.check_kind(kind)?;
        }
        let mut total = 0u64;
        for side in [&self.imports, &self.exports] {
            if side.len() > MAX_ITEMS {
                return Err(Error("prepared world has too many items".into()));
            }
            unique(side.iter().map(|entry| entry.key.as_str()), "world item")?;
            for entry in side {
                identity(&entry.key)?;
                let size = match &entry.item {
                    Item::Interface(interface) => {
                        if let Some(contract) = &interface.contract {
                            identity(contract)?;
                        }
                        if let Some(name) = &interface.name {
                            self::name(name, false)?;
                        }
                        bounded(interface.resources.len())?;
                        for (name, ty) in &interface.resources {
                            self::name(name, false)?;
                            self.check_resource(*ty)?;
                            // A direct definition belongs to this interface. `use` aliases have
                            // their own Alias node and deliberately keep the foreign identity.
                            if let Kind::Resource(resource) = &self.types[*ty as usize]
                                && (resource.interface != interface.contract
                                    || resource.name.as_deref() != Some(name))
                            {
                                return Err(Error(
                                    "resource definition differs from its interface binding".into(),
                                ));
                            }
                        }
                        bounded(interface.functions.len())?;
                        unique(
                            interface
                                .functions
                                .iter()
                                .map(|function| function.name.as_str()),
                            "function",
                        )?;
                        let mut size = 0u64;
                        for (name, ty) in &interface.resources {
                            size = size
                                .saturating_add(name.len() as u64)
                                .saturating_add(self.check_ty(Ty::Ref(*ty), &sizes)?);
                        }
                        for function in &interface.functions {
                            size = size.saturating_add(self.check_function(function, &sizes)?);
                        }
                        size
                    }
                    Item::Function(function) => self.check_function(function, &sizes)?,
                    Item::Type(ty) => self.check_ty(*ty, &sizes)?,
                };
                total = expansion(total.saturating_add(size))?;
            }
        }
        Ok(())
    }

    /// Check one definition's members after its references were ordered.
    fn check_kind(&self, kind: &Kind) -> Result<(), Error> {
        match kind {
            Kind::Record(fields) => {
                bounded(fields.len())?;
                unique(fields.iter().map(|field| field.name.as_str()), "field")?;
                fields.iter().try_for_each(|field| name(&field.name, false))
            }
            Kind::Tuple(types) => bounded(types.len()),
            Kind::Variant(cases) => {
                bounded(cases.len())?;
                unique(cases.iter().map(|case| case.name.as_str()), "case")?;
                cases.iter().try_for_each(|case| name(&case.name, false))
            }
            Kind::Enum(names) | Kind::Flags(names) => {
                bounded(names.len())?;
                unique(names.iter().map(String::as_str), "case")?;
                names.iter().try_for_each(|value| name(value, false))
            }
            Kind::Own(id) | Kind::Borrow(id) => self.check_resource(*id),
            Kind::Resource(resource) => {
                if let Some(interface) = &resource.interface {
                    identity(interface)?;
                    if resource.name.is_none() {
                        return Err(Error(
                            "a named resource interface requires its type name".into(),
                        ));
                    }
                }
                if let Some(resource) = &resource.name {
                    name(resource, false)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Check a signature's names, parameter set and resource scope; return its expanded size.
    fn check_function(&self, function: &Function, sizes: &[u64]) -> Result<u64, Error> {
        name(&function.name, true)?;
        bounded(function.params.len())?;
        unique(
            function.params.iter().map(|param| param.name.as_str()),
            "parameter",
        )?;
        let mut size = 1 + function.name.len() as u64;
        for param in &function.params {
            name(&param.name, false)?;
            size = size
                .saturating_add(param.name.len() as u64)
                .saturating_add(self.check_ty(param.ty, sizes)?);
        }
        if let Some(result) = function.result {
            size = size.saturating_add(self.check_ty(result, sizes)?);
        }
        if let Some(resource) = function.kind.resource() {
            self.check_resource(resource)?;
        }
        Ok(size)
    }

    /// Require a reference to an existing definition and return the type's expanded size.
    fn check_ty(&self, ty: Ty, sizes: &[u64]) -> Result<u64, Error> {
        match ty {
            Ty::Ref(id) => sizes
                .get(id as usize)
                .copied()
                .ok_or_else(|| Error("prepared type reference is out of range".into())),
            _ => Ok(1),
        }
    }

    /// Handles and resource-scoped functions must name a resource through aliases only.
    fn check_resource(&self, mut id: u32) -> Result<(), Error> {
        loop {
            match self.types.get(id as usize) {
                Some(Kind::Resource(_)) => return Ok(()),
                Some(Kind::Alias(Ty::Ref(next))) if *next < id => id = *next,
                _ => return Err(Error("prepared handle does not name a resource".into())),
            }
        }
    }
}

/// Derive the JavaScript member spellings that value validation looks up for one definition.
fn spellings(kind: &Kind) -> Box<[Box<str>]> {
    match kind {
        Kind::Record(fields) => fields
            .iter()
            .map(|field| crate::javascript_name(&field.name).into())
            .collect(),
        Kind::Flags(flags) => flags
            .iter()
            .map(|flag| crate::javascript_name(flag).into())
            .collect(),
        _ => Box::default(),
    }
}

/// Bytes of member names a structural signature spells out for one definition.
fn member_names(kind: &Kind) -> u64 {
    let bytes = |name: &String| name.len() as u64;
    match kind {
        Kind::Record(fields) => fields.iter().map(|field| bytes(&field.name)).sum(),
        Kind::Variant(cases) => cases.iter().map(|case| bytes(&case.name)).sum(),
        Kind::Enum(names) | Kind::Flags(names) => names.iter().map(bytes).sum(),
        Kind::Resource(resource) => resource
            .interface
            .iter()
            .chain(&resource.name)
            .map(bytes)
            .sum(),
        _ => 0,
    }
}

/// Reject a definition or world whose shared references expand beyond the signature budget.
fn expansion(size: u64) -> Result<u64, Error> {
    if size > MAX_EXPANSION {
        return Err(Error(
            "prepared contract signatures expand beyond their size limit".into(),
        ));
    }
    Ok(size)
}

/// Reject oversized member lists.
fn bounded(length: usize) -> Result<(), Error> {
    if length > MAX_MEMBERS {
        return Err(Error("prepared declaration has too many members".into()));
    }
    Ok(())
}

/// Reject duplicate names that would make lookup ambiguous.
fn unique<'a>(names: impl Iterator<Item = &'a str>, kind: &str) -> Result<(), Error> {
    let mut seen = std::collections::BTreeSet::new();
    for name in names {
        if !seen.insert(name) {
            return Err(Error(format!("duplicate prepared {kind} {name}")));
        }
    }
    Ok(())
}

/// WIT identifiers; function names may also use the `[method]resource.name` forms.
fn name(value: &str, function: bool) -> Result<(), Error> {
    let valid = !value.is_empty()
        && value.len() <= MAX_NAME
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || byte == b'-'
                || (function && matches!(byte, b'[' | b']' | b'.'))
        });
    if valid {
        Ok(())
    } else {
        Err(Error(format!("invalid prepared WIT name {value:?}")))
    }
}

/// Package, interface and world-key identities such as `ns:pkg/name@1.2.3`.
fn identity(value: &str) -> Result<(), Error> {
    let valid = !value.is_empty()
        && value.len() <= MAX_NAME
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b':' | b'/' | b'@' | b'.' | b'+')
        });
    if valid {
        Ok(())
    } else {
        Err(Error(format!("invalid prepared WIT identity {value:?}")))
    }
}
