//! Typed wire records for the kernel protocol.
//!
//! Each record is defined once in Rust. Its serde representation is the platform wire shape, and
//! its [`Wire`]/[`Declare`] implementation is the TypeScript declaration emitted by
//! [`crate::typescript`]. Inputs reject unknown fields; identities are range checked while decoding.

use crate::decode::{Decode, Input, expected, integer, invalid};
use serde::{Serialize, Serializer};
use std::collections::{BTreeMap, BTreeSet};
use tembra_core::{self as core, Code, Error, Id, MAX_ID, Owner, Reason, Result};

/// The TypeScript spelling of a wire type.
pub trait Wire {
    /// Optional input fields may be omitted; optional output fields are `null` when absent.
    const OPTIONAL: bool = false;

    /// Return the TypeScript type expression of a decoded result.
    fn ts() -> String;

    /// Return the TypeScript type expression accepted as input; collections may be readonly.
    fn input() -> String {
        Self::ts()
    }
}

/// A named record or union that the generator declares once.
pub trait Declare {
    /// Return the complete TypeScript declaration, ending in a newline.
    fn declaration() -> String;
}

macro_rules! primitive {
    ($($ty:ty => $ts:literal),* $(,)?) => {$(
        impl Wire for $ty {
            fn ts() -> String {
                $ts.into()
            }
        }
    )*};
}

primitive! {
    bool => "boolean",
    i32 => "number",
    i64 => "number",
    u32 => "number",
    u64 => "number",
    usize => "number",
    String => "string",
    () => "null",
}

impl<T: Wire> Wire for Vec<T> {
    fn ts() -> String {
        array(T::ts())
    }

    fn input() -> String {
        format!("readonly {}", array(T::input()))
    }
}

impl<T: Wire> Wire for BTreeSet<T> {
    fn ts() -> String {
        array(T::ts())
    }
}

impl<T: Wire> Wire for BTreeMap<String, T> {
    fn ts() -> String {
        format!("Record<string, {}>", T::ts())
    }

    fn input() -> String {
        format!("Readonly<Record<string, {}>>", T::input())
    }
}

impl<T: Wire> Wire for Option<T> {
    const OPTIONAL: bool = true;

    fn ts() -> String {
        format!("{} | null", T::ts())
    }

    fn input() -> String {
        format!("{} | null", T::input())
    }
}

/// Spell an array without changing the precedence of a union element type.
fn array(element: String) -> String {
    if element.contains(' ') {
        format!("({element})[]")
    } else {
        format!("{element}[]")
    }
}

/// Convert a Rust field name to the camelCase spelling used by serde and TypeScript.
pub(crate) fn camel(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut upper = false;
    for character in name.chars() {
        if character == '_' {
            upper = true;
        } else if upper {
            out.extend(character.to_uppercase());
            upper = false;
        } else {
            out.push(character);
        }
    }
    out
}

/// Field description used by generated declarations: Rust name, optionality and TypeScript type.
pub(crate) type FieldShape = (&'static str, bool, String);

/// Which side of the platform seam a declaration describes.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Direction {
    Input,
    Output,
}

/// Print one field line. Optional inputs may be omitted or explicitly empty.
pub(crate) fn field_line(field: &FieldShape, direction: Direction) -> String {
    let (name, optional, ts) = field;
    let name = camel(name);
    if *optional && direction == Direction::Input {
        format!("  {name}?: {ts} | undefined;\n")
    } else {
        format!("  {name}: {ts};\n")
    }
}

/// Print a TypeScript interface, optionally beginning with a literal discriminant.
pub(crate) fn interface(
    name: &str,
    tag: Option<(&str, &str)>,
    fields: &[FieldShape],
    direction: Direction,
) -> String {
    let mut out = format!("export interface {name} {{\n");
    if let Some((tag, wire)) = tag {
        out += &format!("  {tag}: \"{wire}\";\n");
    }
    for field in fields {
        out += &field_line(field, direction);
    }
    out + "}\n"
}

/// Print a union alias on one line when it fits the formatter width, otherwise one member per line.
pub(crate) fn alias(name: &str, members: &[String]) -> String {
    let line = format!("export type {name} = {};\n", members.join(" | "));
    if line.len() <= 101 {
        return line;
    }
    let mut out = format!("export type {name} =\n");
    for (index, member) in members.iter().enumerate() {
        out += &format!("  | {member}");
        out += if index + 1 == members.len() {
            ";\n"
        } else {
            "\n"
        };
    }
    out
}

/// Define a serialized output record or a strictly decoded input record.
macro_rules! record {
    ($(#[$meta:meta])* output $name:ident { $($field:ident: $ty:ty),* $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, serde::Serialize)]
        #[serde(rename_all = "camelCase")]
        pub struct $name { $(pub $field: $ty,)* }
        record!(@describe $name Output ts { $($field: $ty),* });
    };
    ($(#[$meta:meta])* input $name:ident { $($field:ident: $ty:ty),* $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq)]
        pub struct $name { $(pub $field: $ty,)* }
        decode_record!($name { $($field),* });
        record!(@describe $name Input input { $($field: $ty),* });
    };
    (@describe $name:ident $direction:ident $spelling:ident { $($field:ident: $ty:ty),* }) => {
        impl $crate::wire::Wire for $name {
            fn ts() -> String {
                stringify!($name).into()
            }
        }
        impl $crate::wire::Declare for $name {
            fn declaration() -> String {
                $crate::wire::interface(
                    stringify!($name),
                    None,
                    &[$((
                        stringify!($field),
                        <$ty as $crate::wire::Wire>::OPTIONAL,
                        <$ty as $crate::wire::Wire>::$spelling(),
                    )),*],
                    $crate::wire::Direction::$direction,
                )
            }
        }
    };
}

/// Define a serialized output union discriminated by one literal field, plus its closed names.
macro_rules! union {
    (
        $(#[$meta:meta])* $name:ident: $tag:literal / $kind:ident {
            $($variant:ident = $wire:literal { $($field:ident: $ty:ty),* $(,)? }),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, serde::Serialize)]
        #[serde(tag = $tag)]
        pub enum $name {
            $(
                #[serde(rename = $wire, rename_all = "camelCase")]
                $variant { $($field: $ty),* },
            )+
        }
        names! {
            /// Stable discriminants of this wire union.
            $kind { $($variant => $wire),+ }
        }
        impl $crate::wire::Wire for $name {
            fn ts() -> String {
                stringify!($name).into()
            }
        }
        impl $crate::wire::Declare for $name {
            fn declaration() -> String {
                let mut out = String::new();
                let mut members = Vec::new();
                $(
                    let member = concat!(stringify!($name), stringify!($variant));
                    out += &$crate::wire::interface(
                        member,
                        Some(($tag, $wire)),
                        &[$((
                            stringify!($field),
                            <$ty as $crate::wire::Wire>::OPTIONAL,
                            <$ty as $crate::wire::Wire>::ts(),
                        )),*],
                        $crate::wire::Direction::Output,
                    );
                    out += "\n";
                    members.push(member.to_string());
                )+
                out + &$crate::wire::alias(stringify!($name), &members)
            }
        }
    };
}

/// Decode a host-issued identity that JavaScript can represent exactly.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct Handle(pub Id);

impl Handle {
    /// Decode a scalar platform identity without constructing a generic metadata tree.
    pub fn number(value: f64) -> Result<Self> {
        if value.is_finite() && value.fract() == 0.0 && (0.0..=MAX_ID as f64).contains(&value) {
            Ok(Self(value as Id))
        } else {
            Err(invalid("expected a safe unsigned integer identity"))
        }
    }
}

impl Wire for Handle {
    fn ts() -> String {
        "number".into()
    }
}

impl Decode for Handle {
    fn decode(input: Input) -> Result<Self> {
        integer(&input)
            .and_then(|value| u64::try_from(value).ok())
            .filter(|value| *value <= MAX_ID)
            .map(Handle)
            .ok_or_else(|| expected("a safe unsigned integer identity", &input))
    }
}

/// Collect optional identity lists.
pub(crate) fn ids(list: Option<Vec<Handle>>) -> Vec<Id> {
    list.into_iter().flatten().map(|handle| handle.0).collect()
}

/// A wide clock or revision supplied as decimal text or an unsigned integer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Wide(pub u64);

impl Wire for Wide {
    fn ts() -> String {
        "number | string".into()
    }
}

impl Decode for Wide {
    fn decode(input: Input) -> Result<Self> {
        let value = match &input {
            Input::Text(text) if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) => {
                text.parse().ok()
            }
            // Wider revisions must use decimal text; a platform float may already be rounded.
            Input::Float(value)
                if value.fract() == 0.0 && (0.0..=MAX_ID as f64).contains(value) =>
            {
                Some(*value as u64)
            }
            other => integer(other).and_then(|value| u64::try_from(value).ok()),
        };
        value
            .map(Wide)
            .ok_or_else(|| expected("an unsigned revision or clock", &input))
    }
}

/// A 64-bit revision encoded as decimal text so JavaScript preserves every bit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Decimal(pub u64);

impl Wire for Decimal {
    fn ts() -> String {
        "string".into()
    }
}

impl Serialize for Decimal {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_str(&self.0)
    }
}

/// A host-relative millisecond deadline, delivered as a JavaScript number.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Clock(pub u64);

impl Wire for Clock {
    fn ts() -> String {
        "number".into()
    }
}

impl Serialize for Clock {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        if self.0 <= MAX_ID {
            serializer.serialize_i64(self.0 as i64)
        } else {
            // Deadlines beyond 2^53 ms are not exactly representable in JavaScript either way.
            serializer.serialize_f64(self.0 as f64)
        }
    }
}

/// A host-issued identity in a result. Identities never exceed 2^53 - 1, so they are serialized
/// as signed integers: every platform then produces an exact JavaScript number, never a BigInt.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Ident(pub Id);

impl Wire for Ident {
    fn ts() -> String {
        "number".into()
    }
}

impl Serialize for Ident {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        match i64::try_from(self.0) {
            Ok(value) => serializer.serialize_i64(value),
            Err(_) => serializer.serialize_u64(self.0),
        }
    }
}

impl From<Id> for Ident {
    fn from(id: Id) -> Self {
        Self(id)
    }
}

/// Convert an identity list for a result.
pub(crate) fn idents(ids: impl IntoIterator<Item = Id>) -> Vec<Ident> {
    ids.into_iter().map(Ident).collect()
}

/// Serialize or decode a closed core enumeration through its stable wire name.
macro_rules! label {
    ($name:ident($core:ty) as $ts:literal { $($variant:ident => $wire:literal),+ $(,)? }) => {
        #[doc = concat!("Wire spelling of [`", stringify!($core), "`].")]
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub struct $name(pub $core);
        impl Wire for $name {
            fn ts() -> String {
                $ts.into()
            }
        }
        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
                serializer.serialize_str(match self.0 { $(<$core>::$variant => $wire,)+ })
            }
        }
        impl Decode for $name {
            fn decode(input: Input) -> Result<Self> {
                match &input {
                    $(Input::Text(text) if text == $wire => Ok($name(<$core>::$variant)),)+
                    _ => Err(expected(concat!("one of", $(" ", $wire),+), &input)),
                }
            }
        }
    };
}

label!(LifetimeName(core::Lifetime) as "Lifetime" {
    Referenced => "referenced",
    Host => "host",
});

label!(StatusName(core::Status) as "Status" {
    Loading => "loading",
    Live => "live",
    Released => "released",
    Gone => "gone",
});

label!(ContextKindName(core::ContextKind) as "\"text\" | \"integer\" | \"boolean\" | \"opaque\"" {
    Text => "text",
    Integer => "integer",
    Boolean => "boolean",
    Opaque => "opaque",
});

/// Callback error codes. Codes outside the closed set become `invalid`, as before.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CodeName(pub Code);

impl Wire for CodeName {
    fn ts() -> String {
        "Code".into()
    }
}

impl Serialize for CodeName {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(self.0.as_str())
    }
}

impl Decode for CodeName {
    fn decode(input: Input) -> Result<Self> {
        let code = String::decode(input)?;
        Ok(Self(Code::parse(&code).unwrap_or(Code::Invalid)))
    }
}

/// A scalar context binding. Kind agreement is checked against the key by the command.
#[derive(Clone, Debug, PartialEq)]
pub enum ContextScalar {
    Text(String),
    Integer(i64),
    Boolean(bool),
    /// A number that is not a safe integer; refused with the context reason.
    Number,
}

impl Wire for ContextScalar {
    fn ts() -> String {
        "string | number | boolean".into()
    }
}

/// Largest integer JavaScript represents exactly.
const SAFE_INTEGER: i64 = (1 << 53) - 1;

impl Decode for ContextScalar {
    fn decode(input: Input) -> Result<Self> {
        Ok(match input {
            Input::Text(value) => Self::Text(value),
            Input::Bool(value) => Self::Boolean(value),
            Input::Float(value) if value.fract() == 0.0 && value.abs() <= SAFE_INTEGER as f64 => {
                Self::Integer(value as i64)
            }
            Input::Float(_) => Self::Number,
            other => match integer(&other) {
                Some(value)
                    if (-i128::from(SAFE_INTEGER)..=i128::from(SAFE_INTEGER)).contains(&value) =>
                {
                    Self::Integer(value as i64)
                }
                Some(_) => Self::Number,
                None => return Err(expected("a text, integer or boolean context value", &other)),
            },
        })
    }
}

/// The host, an installed orb or a managed object that owns a declaration.
#[derive(Clone, Debug, PartialEq)]
pub struct OwnerInput {
    pub kind: OwnerKind,
    pub id: Option<Handle>,
}

decode_record!(OwnerInput { kind, id });

names! {
    /// Owner discriminants shared by decoding and generated bindings.
    OwnerKind {
        Host => "host",
        Orb => "orb",
        Object => "object",
    }
}

impl Decode for OwnerKind {
    fn decode(input: Input) -> Result<Self> {
        match &input {
            Input::Text(text) => Self::parse(text),
            _ => None,
        }
        .ok_or_else(|| expected("one of host orb object", &input))
    }
}

impl OwnerInput {
    /// Require an identity exactly for orb and object owners.
    pub fn resolve(&self) -> Result<Owner> {
        match (self.kind, self.id) {
            (OwnerKind::Host, None) => Ok(Owner::Host),
            (OwnerKind::Orb, Some(id)) => Ok(Owner::Orb(id.0)),
            (OwnerKind::Object, Some(id)) => Ok(Owner::Object(id.0)),
            _ => Err(Error::invalid(Reason::Owner, "invalid owner")),
        }
    }
}

impl Wire for OwnerInput {
    fn ts() -> String {
        "Owner".into()
    }
}

impl Declare for OwnerInput {
    fn declaration() -> String {
        format!(
            "export type Owner = {{ kind: {:?} }} | {{ kind: {:?} | {:?}; id: number }};\n",
            OwnerKind::Host.as_str(),
            OwnerKind::Orb.as_str(),
            OwnerKind::Object.as_str(),
        )
    }
}

record! {
    /// Stable typed context key identity.
    input ContextKeyInput { name: String, version: u32, kind: ContextKindName }
}

impl ContextKeyInput {
    /// Convert the decoded key to the core contract.
    pub fn contract(self) -> core::ContextContract {
        core::ContextContract {
            name: self.name,
            version: self.version,
            kind: self.kind.0,
        }
    }
}

record! {
    /// A service declared by an orb.
    input ServiceDeclaration {
        name: String,
        methods: Vec<String>,
        signature: Option<String>,
        lifetime: Option<LifetimeName>,
        linger: Option<u32>,
    }
}

record! {
    /// A dependency on another orb's services and hooks.
    input RequirementDeclaration {
        orb: String,
        api_version: u32,
        services: Option<BTreeMap<String, Vec<String>>>,
        signatures: Option<BTreeMap<String, String>>,
        hooks: Option<Vec<String>>,
    }
}

record! {
    /// An orb contract. Implementations and values stay with the language adapter.
    input OrbDeclaration {
        name: String,
        api_version: u32,
        services: Vec<ServiceDeclaration>,
        requires: Option<Vec<RequirementDeclaration>>,
        hooks: Option<Vec<String>>,
        isolated: Option<bool>,
    }
}

impl OrbDeclaration {
    /// Convert the decoded declaration to the core model.
    pub fn into_core(self) -> core::Orb {
        core::Orb {
            name: self.name,
            api_version: self.api_version,
            services: self
                .services
                .into_iter()
                .map(|service| core::Service {
                    signature: service.signature,
                    name: service.name,
                    methods: service.methods.into_iter().collect(),
                    lifetime: service
                        .lifetime
                        .map_or(core::Lifetime::Referenced, |lifetime| lifetime.0),
                    linger_ms: service.linger.unwrap_or(0),
                })
                .collect(),
            requires: self
                .requires
                .into_iter()
                .flatten()
                .map(|requirement| core::Requirement {
                    signatures: requirement.signatures.unwrap_or_default(),
                    orb: requirement.orb,
                    api_version: requirement.api_version,
                    methods: requirement
                        .services
                        .unwrap_or_default()
                        .into_iter()
                        .map(|(service, methods)| (service, methods.into_iter().collect()))
                        .collect(),
                    hooks: requirement.hooks.into_iter().flatten().collect(),
                })
                .collect(),
            hooks: self.hooks.into_iter().flatten().collect(),
            isolated: self.isolated.unwrap_or(false),
        }
    }
}

record! {
    /// Adopt a migrated state value when committing a reload.
    input MigrationInput { object: Handle, revision: Wide, value: Handle, version: u32 }
}

record! {
    /// One previous/next scope pairing for an explicit reload.
    input ReplacementInput { previous: Option<Handle>, next: Option<Handle> }
}

record! {
    /// A structured callback failure.
    input ErrorInput { code: CodeName, reason: String, message: String }
}

record! {
    /// The physical settlement of one requested effect.
    input CompletionInput { value: Option<Handle>, error: Option<ErrorInput> }
}

impl From<CompletionInput> for core::Completion {
    fn from(completion: CompletionInput) -> Self {
        Self {
            value: completion.value.map(|handle| handle.0),
            error: completion
                .error
                .map(|error| Error::new(error.code.0, error.reason, error.message)),
        }
    }
}

record! {
    /// Capabilities compiled into this kernel build.
    output Features { hooks: bool, subscriptions: bool, views: bool, telemetry: bool }
}

record! {
    /// Context frame counters.
    output ContextStats { live: usize, peak: usize, keys: usize }
}

union! {
    /// The nearest binding of a typed context key.
    ContextEntry: "kind" / ContextEntryKind {
        Text = "text" { value: String },
        Integer = "integer" { value: i64 },
        Boolean = "boolean" { value: bool },
        Opaque = "opaque" { source: Ident },
    }
}

record! {
    /// A structured kernel failure.
    output KernelError { code: CodeName, reason: String, message: String }
}

impl From<Error> for KernelError {
    fn from(error: Error) -> Self {
        Self {
            code: CodeName(error.code),
            reason: error.reason,
            message: error.message,
        }
    }
}

record! {
    /// A declared service.
    output ServiceInfo {
        name: String,
        methods: Vec<String>,
        signature: Option<String>,
        lifetime: LifetimeName,
        linger: u32,
    }
}

record! {
    /// A declared dependency.
    output RequirementInfo {
        orb: String,
        api_version: u32,
        services: BTreeMap<String, Vec<String>>,
        signatures: BTreeMap<String, String>,
        hooks: Vec<String>,
    }
}

record! {
    /// An installed orb version without its implementation.
    output KernelOrb {
        id: Ident,
        owner: Ident,
        code_version: u32,
        name: String,
        api_version: u32,
        services: Vec<ServiceInfo>,
        hooks: Vec<String>,
        requires: Vec<RequirementInfo>,
        isolated: bool,
    }
}

impl From<&core::InstalledOrb> for KernelOrb {
    fn from(orb: &core::InstalledOrb) -> Self {
        let declaration = &orb.declaration;
        Self {
            id: Ident(orb.id),
            owner: Ident(orb.owner),
            code_version: orb.code_version,
            name: declaration.name.clone(),
            api_version: declaration.api_version,
            services: declaration
                .services
                .iter()
                .map(|service| ServiceInfo {
                    name: service.name.clone(),
                    methods: service.methods.iter().cloned().collect(),
                    signature: service.signature.clone(),
                    lifetime: LifetimeName(service.lifetime),
                    linger: service.linger_ms,
                })
                .collect(),
            hooks: declaration.hooks.iter().cloned().collect(),
            requires: declaration
                .requires
                .iter()
                .map(|requirement| RequirementInfo {
                    orb: requirement.orb.clone(),
                    api_version: requirement.api_version,
                    services: requirement
                        .methods
                        .iter()
                        .map(|(service, methods)| {
                            (service.clone(), methods.iter().cloned().collect())
                        })
                        .collect(),
                    signatures: requirement.signatures.clone(),
                    hooks: requirement.hooks.iter().cloned().collect(),
                })
                .collect(),
            isolated: declaration.isolated,
        }
    }
}

record! {
    /// A prepared reload candidate.
    output Reload {
        id: Ident,
        code_version: u32,
        orbs: Vec<KernelOrb>,
        objects: Vec<Ident>,
        providers: BTreeMap<String, Ident>,
    }
}

impl From<core::Reload> for Reload {
    fn from(reload: core::Reload) -> Self {
        Self {
            id: Ident(reload.id),
            code_version: reload.code_version,
            orbs: reload.orbs.iter().map(KernelOrb::from).collect(),
            objects: idents(reload.objects),
            providers: reload
                .providers
                .into_iter()
                .map(|(service, id)| (service, Ident(id)))
                .collect(),
        }
    }
}

record! {
    /// A managed object's stable identity.
    output AddressInfo { service: String, key: Vec<String>, principal: String }
}

record! {
    /// A new reference claim.
    output Acquisition { reference: Ident, object: Ident, orb: Ident, code_version: u32, activate: bool }
}

record! {
    /// An admitted call and its retained implementation version.
    output Invocation { id: Ident, root: Ident, object: Ident, orb: Ident, code_version: u32, method: String }
}

impl From<core::Invocation> for Invocation {
    fn from(invocation: core::Invocation) -> Self {
        Self {
            id: Ident(invocation.id),
            root: Ident(invocation.root),
            object: Ident(invocation.object),
            orb: Ident(invocation.orb),
            code_version: invocation.code_version,
            method: invocation.method,
        }
    }
}

record! {
    /// One admitted hook observation and its contribution plan.
    output HookObservation { invocation: Invocation, plan: Vec<Ident> }
}

record! {
    /// Immutable state metadata.
    output Snapshot { value: Ident, revision: Decimal, version: u32 }
}

impl From<core::Snapshot> for Snapshot {
    fn from(snapshot: core::Snapshot) -> Self {
        Self {
            value: Ident(snapshot.value),
            revision: Decimal(snapshot.revision),
            version: snapshot.version,
        }
    }
}

record! {
    /// A resource claim and its reusable value, when one is already open.
    output ResourceClaim { id: Ident, value: Option<Ident> }
}

record! {
    /// Kernel ownership counters.
    output Diagnostics {
        objects: usize,
        references: usize,
        roots: usize,
        calls: usize,
        resources: usize,
        tasks: usize,
        orbs: usize,
        closing: bool,
        code_version: u32,
    }
}

record! {
    /// One scope replacement inside an adoption effect.
    output Replacement { previous: Option<Ident>, next: Option<Ident> }
}

union! {
    /// A language callback requested by a lifecycle program.
    Effect: "type" / EffectType {
        Stop = "stop" { scope: Ident, keep_listeners: bool },
        Open = "open" { resource: Ident },
        Close = "close" { resource: Ident },
        Restore = "restore" { previous: Ident, resource: Ident },
        Prepare = "prepare" { scope: Ident, previous: Option<Ident> },
        Validate = "validate" { scope: Ident, value: Ident },
        Initialize = "initialize" { scope: Ident },
        Migrate = "migrate" { scope: Ident, value: Option<Ident>, version: u32 },
        Adopt = "adopt" { replacements: Vec<Replacement> },
        Start = "start" { scope: Ident },
        Drop = "drop" { scope: Ident },
    }
}

impl From<core::Effect> for Effect {
    fn from(effect: core::Effect) -> Self {
        use core::Effect as E;
        let id = Ident;
        let optional = |value: Option<Id>| value.map(Ident);
        match effect {
            E::Stop {
                scope,
                keep_listeners,
            } => Self::Stop {
                scope: id(scope),
                keep_listeners,
            },
            E::Open { resource } => Self::Open {
                resource: id(resource),
            },
            E::Close { resource } => Self::Close {
                resource: id(resource),
            },
            E::Restore { previous, resource } => Self::Restore {
                previous: id(previous),
                resource: id(resource),
            },
            E::Prepare { scope, previous } => Self::Prepare {
                scope: id(scope),
                previous: optional(previous),
            },
            E::Validate { scope, value } => Self::Validate {
                scope: id(scope),
                value: id(value),
            },
            E::Initialize { scope } => Self::Initialize { scope: id(scope) },
            E::Migrate {
                scope,
                value,
                version,
            } => Self::Migrate {
                scope: id(scope),
                value: optional(value),
                version,
            },
            E::Adopt { replacements } => Self::Adopt {
                replacements: replacements
                    .into_iter()
                    .map(|pair| Replacement {
                        previous: optional(pair.previous),
                        next: optional(pair.next),
                    })
                    .collect(),
            },
            E::Start { scope } => Self::Start { scope: id(scope) },
            E::Drop { scope } => Self::Drop { scope: id(scope) },
        }
    }
}

/// Convert an effect list.
fn effects(effects: Vec<core::Effect>) -> Vec<Effect> {
    effects.into_iter().map(Effect::from).collect()
}

union! {
    /// Work for the owning language adapter after a kernel operation completes.
    Action: "type" / ActionType {
        Execute = "execute" { id: Ident, effects: Vec<Effect> },
        CancelEffects = "cancel-effects" { id: Ident, effects: Vec<Effect> },
        ProgramPrepared = "program-prepared" { id: Ident },
        ProgramComplete = "program-complete" { id: Ident, error: Option<KernelError> },
        Dispose = "dispose" { id: Ident },
        CloseResource = "close-resource" { id: Ident },
        DropValue = "drop-value" { id: Ident },
        CancelRoot = "cancel-root" { id: Ident },
        CancelTask = "cancel-task" { id: Ident },
        RetireOrb = "retire-orb" { id: Ident },
        State = "state" { id: Ident, revision: Decimal },
        SubscriptionFailed = "subscription-failed" { id: Ident },
        DetachSubscription = "detach-subscription" { id: Ident },
        Deliver = "deliver" { id: Ident, payload: Ident, operation: Ident },
    }
}

impl From<core::Action> for Action {
    fn from(action: core::Action) -> Self {
        use core::Action as A;
        match action {
            A::Execute {
                program,
                effects: e,
            } => Self::Execute {
                id: Ident(program),
                effects: effects(e),
            },
            A::CancelEffects {
                program,
                effects: e,
            } => Self::CancelEffects {
                id: Ident(program),
                effects: effects(e),
            },
            A::ProgramPrepared(id) => Self::ProgramPrepared { id: Ident(id) },
            A::ProgramComplete { program, error } => Self::ProgramComplete {
                id: Ident(program),
                error: error.map(KernelError::from),
            },
            A::DisposeObject(id) => Self::Dispose { id: Ident(id) },
            A::CloseResource(id) => Self::CloseResource { id: Ident(id) },
            A::DropValue(id) => Self::DropValue { id: Ident(id) },
            A::CancelRoot(id) => Self::CancelRoot { id: Ident(id) },
            A::CancelTask(id) => Self::CancelTask { id: Ident(id) },
            A::RetireOrb(id) => Self::RetireOrb { id: Ident(id) },
            A::StateChanged { object, revision } => Self::State {
                id: Ident(object),
                revision: Decimal(revision),
            },
            #[cfg(feature = "subscriptions")]
            A::SubscriptionFailed(id) => Self::SubscriptionFailed { id: Ident(id) },
            #[cfg(feature = "subscriptions")]
            A::DetachSubscription(id) => Self::DetachSubscription { id: Ident(id) },
            #[cfg(feature = "subscriptions")]
            A::Deliver {
                subscription,
                payload,
                operation,
            } => Self::Deliver {
                id: Ident(subscription),
                payload: Ident(payload),
                operation: Ident(operation),
            },
        }
    }
}

union! {
    /// The next step of a lifecycle program.
    Step: "type" / StepType {
        Execute = "execute" { effects: Vec<Effect> },
        Wait = "wait" { deadline: Option<Decimal> },
        Prepared = "prepared" {},
        Complete = "complete" {},
        Failed = "failed" { error: KernelError },
    }
}

impl From<core::Step> for Step {
    fn from(step: core::Step) -> Self {
        match step {
            core::Step::Execute(e) => Self::Execute {
                effects: effects(e),
            },
            core::Step::Wait { deadline } => Self::Wait {
                deadline: deadline.map(Decimal),
            },
            core::Step::Prepared => Self::Prepared {},
            core::Step::Complete => Self::Complete {},
            core::Step::Failed(error) => Self::Failed {
                error: error.into(),
            },
        }
    }
}

record! {
    /// Actions and the next deadline after one physical call finishes.
    output CallSettlement { actions: Vec<Action>, deadline: Option<Clock> }
}

/// Every named declaration, in the order the TypeScript generator emits them.
pub(crate) const DECLARATIONS: &[fn() -> String] = &[
    Features::declaration,
    ContextStats::declaration,
    ContextEntry::declaration,
    KernelError::declaration,
    ServiceInfo::declaration,
    RequirementInfo::declaration,
    KernelOrb::declaration,
    Reload::declaration,
    AddressInfo::declaration,
    Acquisition::declaration,
    Invocation::declaration,
    HookObservation::declaration,
    Snapshot::declaration,
    ResourceClaim::declaration,
    Diagnostics::declaration,
    Replacement::declaration,
    Effect::declaration,
    Action::declaration,
    Step::declaration,
    CallSettlement::declaration,
    OwnerInput::declaration,
    ContextKeyInput::declaration,
    ServiceDeclaration::declaration,
    RequirementDeclaration::declaration,
    OrbDeclaration::declaration,
    MigrationInput::declaration,
    ReplacementInput::declaration,
    ErrorInput::declaration,
    CompletionInput::declaration,
];
