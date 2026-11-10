//! Generate native typed values and standard trappable-result conversions.
//!
//! Named records, variants, enums and flags get one generated encoder/decoder function each,
//! so a type used by many methods (such as `error-code`) is emitted once.
use super::*;
use std::fmt::Write as _;

/// Little-endian scalar widths decoded with `from_le_bytes`.
fn scalar(ty: Type) -> Option<(&'static str, usize)> {
    Some(match ty {
        Type::U16 => ("u16", 2),
        Type::S8 => ("i8", 1),
        Type::S16 => ("i16", 2),
        Type::S32 => ("i32", 4),
        Type::S64 => ("i64", 8),
        Type::F32 => ("f32", 4),
        Type::F64 => ("f64", 8),
        _ => return None,
    })
}

impl<'a> Generator<'a> {
    /// A named type that gets a shared helper function, if it carries no handle.
    fn helper(&self, ty: Type) -> Option<TypeId> {
        let Type::Id(id) = self.peel(ty) else {
            return None;
        };
        let named = matches!(
            self.resolve.types[id].kind,
            TypeDefKind::Record(_)
                | TypeDefKind::Variant(_)
                | TypeDefKind::Enum(_)
                | TypeDefKind::Flags(_)
        );
        (named && !qualify::carries_handle(self.resolve, Type::Id(id))).then_some(id)
    }

    /// Rust statements writing `value` (an owned expression) to `w`; handles use `h`.
    pub(super) fn encode(&mut self, ty: Type, value: &str) -> String {
        let Some(id) = self.helper(ty) else {
            return self.encode_inline(ty, value);
        };
        let name = format!("enc_{}", id.index());
        if self.helpers_seen.insert(name.clone()) {
            let body = self.encode_inline(Type::Id(id), "value");
            let path = self.rust_path(id);
            let _ = writeln!(
                self.helpers,
                "/// Encode `{}`.\n#[allow(unused_variables, clippy::all)]\nfn {name}(w: &mut Writer, value: {path}) {{ {body} }}",
                self.qualified(id)
            );
        }
        format!("{name}(w, {value});")
    }

    fn encode_inline(&mut self, ty: Type, value: &str) -> String {
        match self.peel(ty) {
            Type::Bool => format!("w.u8({value} as u8);"),
            Type::U8 => format!("w.u8({value});"),
            Type::U16 => format!("w.raw(&{value}.to_le_bytes());"),
            Type::U32 | Type::Char => format!("w.u32({value} as u32);"),
            Type::U64 => format!("w.u64({value});"),
            Type::S8 | Type::S16 | Type::S32 | Type::S64 | Type::F32 | Type::F64 => {
                format!("w.raw(&{value}.to_le_bytes());")
            }
            Type::String => format!("w.bytes({value}.as_bytes());"),
            Type::ErrorContext => unreachable!(),
            Type::Id(id) => match self.resolve.types[id].kind.clone() {
                TypeDefKind::List(Type::U8) => format!("w.bytes(&{value});"),
                TypeDefKind::List(element) => format!(
                    "{{ let items = {value}; w.u32(items.len() as u32); for item in items {{ {} }} }}",
                    self.encode(element, "item")
                ),
                TypeDefKind::Tuple(tuple) => {
                    let names: Vec<String> =
                        (0..tuple.types.len()).map(|i| format!("t{i}")).collect();
                    let mut out = format!("{{ let ({},) = {value};", names.join(", "));
                    for (name, ty) in names.iter().zip(&tuple.types) {
                        out += &self.encode(*ty, name);
                    }
                    out + " }"
                }
                TypeDefKind::Record(record) => {
                    let mut out = format!("{{ let record = {value};");
                    for field in &record.fields {
                        out += &self.encode(field.ty, &format!("record.{}", snake(&field.name)));
                    }
                    out + " }"
                }
                TypeDefKind::Flags(flags) => {
                    let path = self.rust_path(id);
                    let mut out = format!("{{ let flags = {value}; let mut bits = 0u32;");
                    for (index, flag) in flags.flags.iter().enumerate() {
                        let constant = snake(&flag.name).to_uppercase();
                        let _ = write!(
                            out,
                            " if flags.contains({path}::{constant}) {{ bits |= 1 << {index}; }}"
                        );
                    }
                    out + " w.u32(bits); }"
                }
                TypeDefKind::Enum(cases) => {
                    let path = self.rust_path(id);
                    let arms: Vec<String> = cases
                        .cases
                        .iter()
                        .enumerate()
                        .map(|(index, case)| format!("{path}::{} => {index}", camel(&case.name)))
                        .collect();
                    format!("w.u32(match {value} {{ {} }});", arms.join(", "))
                }
                TypeDefKind::Variant(variant) => {
                    let path = self.rust_path(id);
                    let mut out = format!("match {value} {{");
                    for (index, case) in variant.cases.iter().enumerate() {
                        let name = camel(&case.name);
                        match case.ty {
                            Some(ty) => {
                                let _ = write!(
                                    out,
                                    " {path}::{name}(payload) => {{ w.u32({index}); {} }}",
                                    self.encode(ty, "payload")
                                );
                            }
                            None => {
                                let _ = write!(out, " {path}::{name} => w.u32({index}),");
                            }
                        }
                    }
                    out + " }"
                }
                TypeDefKind::Option(inner) => format!(
                    "match {value} {{ None => w.u8(0), Some(inner) => {{ w.u8(1); {} }} }}",
                    self.encode(inner, "inner")
                ),
                TypeDefKind::Result(result) => {
                    let ok = result.ok.map_or(String::new(), |ty| self.encode(ty, "ok"));
                    let err = result
                        .err
                        .map_or(String::new(), |ty| self.encode(ty, "err"));
                    let ok_binding = if result.ok.is_some() { "ok" } else { "()" };
                    let err_binding = if result.err.is_some() { "err" } else { "()" };
                    format!(
                        "match {value} {{ Ok({ok_binding}) => {{ w.u8(0); {ok} }} Err({err_binding}) => {{ w.u8(1); {err} }} }}"
                    )
                }
                // Issuing never fails mid-value: a refused handle is journaled so the whole
                // reply rolls back and every upstream resource in it is released.
                TypeDefKind::Handle(Handle::Own(resource))
                | TypeDefKind::Handle(Handle::Borrow(resource)) => {
                    let kind = self.resource(resource);
                    format!("w.u64(h.own({kind}, {value}));")
                }
                TypeDefKind::Resource => {
                    let kind = self.resource(id);
                    format!("w.u64(h.own({kind}, {value}));")
                }
                other => panic!("WIT type {other:?} cannot be encoded by this rule"),
            },
        }
    }

    /// Rust expression decoding one value from `r`; borrows are validated against `h`.
    pub(super) fn decode(&mut self, ty: Type) -> String {
        let Some(id) = self.helper(ty) else {
            return self.decode_inline(ty);
        };
        let name = format!("dec_{}", id.index());
        if self.helpers_seen.insert(name.clone()) {
            let body = self.decode_inline(Type::Id(id));
            let path = self.rust_path(id);
            let _ = writeln!(
                self.helpers,
                "/// Decode and validate `{}`.\n#[allow(unused_variables, clippy::all)]\nfn {name}(r: &mut Reader<'_>) -> wasmtime::Result<{path}> {{ Ok({body}) }}",
                self.qualified(id)
            );
        }
        format!("{name}(r)?")
    }

    fn decode_inline(&mut self, ty: Type) -> String {
        let ty = self.peel(ty);
        if let Some((rust, width)) = scalar(ty) {
            return format!("{rust}::from_le_bytes(r.take({width})?.try_into().expect(\"width\"))");
        }
        match ty {
            Type::Bool => "(r.u8()? != 0)".into(),
            Type::U8 => "r.u8()?".into(),
            Type::U32 => "r.u32()?".into(),
            Type::U64 => "r.u64()?".into(),
            Type::Char => {
                "char::from_u32(r.u32()?).ok_or_else(|| invalid(\"invalid char\"))?".into()
            }
            Type::String => "r.string()?".into(),
            Type::Id(id) => match self.resolve.types[id].kind.clone() {
                TypeDefKind::List(Type::U8) => "r.bytes()?.to_vec()".into(),
                // Every element encodes at least one byte, so the count is bounded by the
                // packet before anything is allocated.
                TypeDefKind::List(element) => format!(
                    "{{ let count = r.u32()? as usize; if count > r.0.len() {{ return Err(invalid(\"list length exceeds the packet\")); }} let mut items = Vec::with_capacity(count); for _ in 0..count {{ items.push({}); }} items }}",
                    self.decode(element)
                ),
                TypeDefKind::Flags(flags) => {
                    let path = self.rust_path(id);
                    let mut out = format!(
                        "{{ let bits = r.u32()?; if bits >> {} != 0 {{ return Err(invalid(\"unknown flag\")); }} let mut flags = {path}::empty();",
                        flags.flags.len()
                    );
                    for (index, flag) in flags.flags.iter().enumerate() {
                        let constant = snake(&flag.name).to_uppercase();
                        let _ = write!(
                            out,
                            " if bits & (1 << {index}) != 0 {{ flags |= {path}::{constant}; }}"
                        );
                    }
                    out + " flags }"
                }
                TypeDefKind::Enum(cases) => {
                    let path = self.rust_path(id);
                    let mut out = "match r.u32()? {".to_owned();
                    for (index, case) in cases.cases.iter().enumerate() {
                        let _ = write!(out, " {index} => {path}::{},", camel(&case.name));
                    }
                    out + " _ => return Err(invalid(\"unknown enum case\")) }"
                }
                TypeDefKind::Variant(variant) => {
                    let path = self.rust_path(id);
                    let mut out = "match r.u32()? {".to_owned();
                    for (index, case) in variant.cases.iter().enumerate() {
                        let name = camel(&case.name);
                        match case.ty {
                            Some(ty) => {
                                let _ =
                                    write!(out, " {index} => {path}::{name}({}),", self.decode(ty));
                            }
                            None => {
                                let _ = write!(out, " {index} => {path}::{name},");
                            }
                        }
                    }
                    out + " _ => return Err(invalid(\"unknown variant case\")) }"
                }
                TypeDefKind::Record(record) => {
                    let path = self.rust_path(id);
                    let mut out = "{".to_owned();
                    let mut fields = Vec::new();
                    for (index, field) in record.fields.iter().enumerate() {
                        let _ = write!(out, " let f{index} = {};", self.decode(field.ty));
                        fields.push(format!("{}: f{index}", snake(&field.name)));
                    }
                    out + &format!(" {path} {{ {} }} }}", fields.join(", "))
                }
                TypeDefKind::Tuple(tuple) => {
                    let mut out = "{".to_owned();
                    let mut names = Vec::new();
                    for (index, ty) in tuple.types.iter().enumerate() {
                        let _ = write!(out, " let t{index} = {};", self.decode(*ty));
                        names.push(format!("t{index}"));
                    }
                    out + &format!(" ({},) }}", names.join(", "))
                }
                TypeDefKind::Option(inner) => format!(
                    "match r.u8()? {{ 0 => None, 1 => Some({}), _ => return Err(invalid(\"unknown option tag\")) }}",
                    self.decode(inner)
                ),
                TypeDefKind::Result(result) => {
                    let ok = result.ok.map_or("()".to_owned(), |ty| self.decode(ty));
                    let err = result.err.map_or("()".to_owned(), |ty| self.decode(ty));
                    format!(
                        "match r.u8()? {{ 0 => Ok({ok}), 1 => Err({err}), _ => return Err(invalid(\"unknown result tag\")) }}"
                    )
                }
                TypeDefKind::Handle(Handle::Borrow(resource)) => {
                    let kind = self.resource(resource);
                    format!("h.borrow({kind}, r.u64()?)?")
                }
                // Claimed now; removed from the table only by `h.commit` after validation.
                TypeDefKind::Handle(Handle::Own(resource)) => {
                    let kind = self.resource(resource);
                    let path = self.resources[kind].1.clone();
                    format!("h.claim_own::<{path}>({kind}, r.u64()?, &mut seen)?")
                }
                // Claimed now, transferred only after the whole packet validated.
                TypeDefKind::Stream(Some(Type::U8)) => "h.claim(r.u64()?, &mut seen)?".into(),
                TypeDefKind::Future(Some(_)) if qualify::writable_future(self.resolve, ty) => {
                    let index = self.ts(ty);
                    format!("h.claim_future({index}, r.u64()?, &mut seen)?")
                }
                other => panic!("WIT parameter type {other:?} has no decode rule"),
            },
            other => panic!("WIT parameter type {other:?} has no decode rule"),
        }
    }

    /// Resource kind of a direct owned handle parameter, if `ty` is one.
    pub(super) fn owned_param(&mut self, ty: Type) -> Option<usize> {
        let Type::Id(id) = self.peel(ty) else {
            return None;
        };
        match self.resolve.types[id].kind {
            TypeDefKind::Handle(Handle::Own(resource)) => Some(self.resource(resource)),
            _ => None,
        }
    }

    /// Statements handing a committed stream or writable future claim (possibly optional) to
    /// upstream as its readable end; `None` for parameters that transfer nothing.
    pub(super) fn transfer(&self, ty: Type, local: &str) -> Option<String> {
        let Type::Id(id) = self.peel(ty) else {
            return None;
        };
        let (inner, optional) = match self.resolve.types[id].kind {
            TypeDefKind::Option(inner) => (self.peel(inner), true),
            _ => (Type::Id(id), false),
        };
        let Type::Id(inner) = inner else {
            return None;
        };
        let make = match self.resolve.types[inner].kind {
            TypeDefKind::Stream(Some(Type::U8)) => {
                "wasmtime::component::StreamReader::new(store.as_context_mut(), claim.transfer(files.pop())?)"
            }
            TypeDefKind::Future(Some(_)) => "claim.reader(store.as_context_mut())",
            _ => return None,
        };
        Some(if optional {
            format!("let {local} = match {local} {{ Some(claim) => Some({make}?), None => None }};")
        } else {
            format!("let {local} = {{ let claim = {local}; {make}? }};")
        })
    }

    /// Rust type of a WIT value as upstream bindgen spells it.
    pub(super) fn rust_type(&mut self, ty: Type) -> String {
        match self.peel(ty) {
            Type::Bool => "bool".into(),
            Type::U8 => "u8".into(),
            Type::U16 => "u16".into(),
            Type::U32 => "u32".into(),
            Type::U64 => "u64".into(),
            Type::S8 => "i8".into(),
            Type::S16 => "i16".into(),
            Type::S32 => "i32".into(),
            Type::S64 => "i64".into(),
            Type::F32 => "f32".into(),
            Type::F64 => "f64".into(),
            Type::Char => "char".into(),
            Type::String => "String".into(),
            Type::ErrorContext => unreachable!(),
            Type::Id(id) => match self.resolve.types[id].kind.clone() {
                TypeDefKind::List(element) => format!("Vec<{}>", self.rust_type(element)),
                TypeDefKind::Tuple(tuple) => {
                    let types: Vec<String> =
                        tuple.types.iter().map(|ty| self.rust_type(*ty)).collect();
                    format!("({},)", types.join(", "))
                }
                TypeDefKind::Option(inner) => format!("Option<{}>", self.rust_type(inner)),
                TypeDefKind::Result(result) => format!(
                    "Result<{}, {}>",
                    result.ok.map_or("()".into(), |ty| self.rust_type(ty)),
                    result.err.map_or("()".into(), |ty| self.rust_type(ty))
                ),
                TypeDefKind::Handle(Handle::Own(resource)) => {
                    let kind = self.resource(resource);
                    format!("Resource<{}>", self.resources[kind].1)
                }
                TypeDefKind::Record(_)
                | TypeDefKind::Variant(_)
                | TypeDefKind::Enum(_)
                | TypeDefKind::Flags(_) => self.rust_path(id),
                other => panic!("WIT type {other:?} has no Rust type rule"),
            },
        }
    }

    /// Resource kind of a borrowed handle parameter, if `ty` is one.
    pub(super) fn borrowed(&mut self, ty: Type) -> Option<usize> {
        let Type::Id(id) = self.peel(ty) else {
            return None;
        };
        match self.resolve.types[id].kind {
            TypeDefKind::Handle(Handle::Borrow(resource)) => Some(self.resource(resource)),
            _ => None,
        }
    }

    /// Owned resources a result issues outside lists, prepaid from the open-file budget at
    /// dispatch so a refusal happens before any upstream side effect.
    pub(super) fn owned(&self, ty: Type) -> usize {
        let Type::Id(id) = self.peel(ty) else {
            return 0;
        };
        match &self.resolve.types[id].kind {
            TypeDefKind::Handle(Handle::Own(resource)) => usize::from(self.budgeted(*resource)),
            TypeDefKind::Resource => usize::from(self.budgeted(id)),
            TypeDefKind::Result(result) => {
                let ok = result.ok.map_or(0, |ty| self.owned(ty));
                ok.max(result.err.map_or(0, |ty| self.owned(ty)))
            }
            TypeDefKind::Option(inner) => self.owned(*inner),
            TypeDefKind::Tuple(tuple) => tuple.types.iter().map(|ty| self.owned(*ty)).sum(),
            _ => 0,
        }
    }

    /// Whether a resource type belongs to a budgeted package (filesystem descriptors, sockets),
    /// whose open handles hold a unit of the open-file budget. Terminal and other handles never
    /// do.
    pub(super) fn budgeted(&self, resource: TypeId) -> bool {
        let Type::Id(resource) = self.peel(Type::Id(resource)) else {
            return false;
        };
        let TypeOwner::Interface(owner) = self.resolve.types[resource].owner else {
            return false;
        };
        let package = self.resolve.interfaces[owner].package.expect("package");
        let name = &self.resolve.packages[package].name.name;
        VIEWS
            .iter()
            .any(|(view, _, _, _, budgeted)| *budgeted && view == name)
    }

    /// `budgeted` for a registered resource kind.
    pub(super) fn budgeted_kind(&self, kind: usize) -> bool {
        let path = &self.resources[kind].1;
        VIEWS.iter().any(|(view, _, _, _, budgeted)| {
            *budgeted && path.contains(&format!("p3::bindings::{view}::"))
        })
    }

    /// Statements encoding a top-level result with upstream trappable error handling.
    pub(super) fn encode_return(&mut self, ty: Option<Type>) -> String {
        let Some(ty) = ty else {
            return "result?;".into();
        };
        if let Type::Id(id) = self.peel(ty)
            && let TypeDefKind::Result(result) = self.resolve.types[id].kind.clone()
            && let Some(Type::Id(error)) = result.err.map(|ty| self.peel(ty))
            && TRAPPABLE.contains(&self.qualified(error).as_str())
        {
            let ok = result.ok.map_or(String::new(), |ty| self.encode(ty, "ok"));
            let ok_binding = if result.ok.is_some() { "ok" } else { "()" };
            let err = self.encode(Type::Id(error), "err");
            return format!(
                "match result {{ Ok({ok_binding}) => {{ w.u8(0); {ok} }} Err(error) => {{ let err = error.downcast()?; w.u8(1); {err} }} }}"
            );
        }
        let encode = self.encode(ty, "value");
        format!("let value = result?; {encode}")
    }
}
