//! The WIT text derived from the module items, and the consumed packages it refers to.
use crate::{Service, Shape};
use heck::{ToKebabCase, ToUpperCamelCase};
use proc_macro2::Span;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use syn::{
    Attribute, Expr, Fields, Ident, ItemEnum, ItemStruct, Lit, Meta, Type, spanned::Spanned,
};
use wit_parser::{Resolve, TypeDefKind};

/// WIT names are kebab-case; Rust identifiers map to them one by one.
pub(crate) fn kebab(ident: &Ident) -> String {
    ident.to_string().to_kebab_case()
}

/// Which kernel interfaces the world uses.
pub(crate) struct Kernel {
    pub state: bool,
    pub resources: bool,
    pub context: bool,
}

/// Write the package: one interface per service, the types in the first interface and borrowed by
/// the others with `use`, then a world using the kernel's interfaces for its states and
/// resources, importing and exporting every contributed hook, and exporting every interface.
pub(crate) fn render<'a>(
    package: &str,
    world: &str,
    shapes: &[Shape],
    services: &[Service],
    hooks: impl Iterator<Item = &'a String>,
    kernel: Kernel,
) -> String {
    let mut wit = format!("package {package};\n");
    for (index, service) in services.iter().enumerate() {
        wit.push('\n');
        for doc in &service.docs {
            wit.push_str(&format!("///{doc}\n"));
        }
        wit.push_str(&format!("interface {} {{\n", kebab(&service.ident)));
        if index == 0 {
            for shape in shapes {
                wit.push_str(&shape.wit);
            }
        } else if !shapes.is_empty() {
            let names = shapes
                .iter()
                .map(|shape| kebab(&shape.ident))
                .collect::<Vec<_>>()
                .join(", ");
            wit.push_str(&format!(
                "  use {}.{{{names}}};\n",
                kebab(&services[0].ident)
            ));
        }
        for function in &service.functions {
            for doc in &function.docs {
                wit.push_str(&format!("  ///{doc}\n"));
            }
            let params = function
                .params
                .iter()
                .map(|(ident, _, wit)| format!("{}: {wit}", kebab(ident)))
                .collect::<Vec<_>>()
                .join(", ");
            let result = function
                .result
                .as_ref()
                .map(|(_, wit)| format!(" -> {wit}"))
                .unwrap_or_default();
            wit.push_str(&format!(
                "  {}: func({params}){result};\n",
                kebab(&function.ident)
            ));
        }
        wit.push_str("}\n");
    }
    wit.push_str(&format!("\nworld {world} {{\n"));
    if kernel.state {
        wit.push_str(&format!(
            "  import {GUEST_STATE};\n  export {GUEST_STATE_INIT};\n"
        ));
    }
    if kernel.resources {
        wit.push_str(&format!(
            "  import {GUEST_RESOURCES};\n  export {GUEST_RESOURCES_INIT};\n"
        ));
    }
    if kernel.context {
        wit.push_str(&format!("  import {GUEST_CONTEXT};\n"));
    }
    for hook in hooks {
        wit.push_str(&format!("  import {hook};\n  export {hook};\n"));
    }
    for service in services {
        wit.push_str(&format!("  export {};\n", kebab(&service.ident)));
    }
    wit.push_str("}\n");
    wit
}

/// The kernel's interfaces, as the world names them.
const GUEST_STATE: &str = "tembra:guest/state@1.0.0";
const GUEST_STATE_INIT: &str = "tembra:guest/state-init@1.0.0";
const GUEST_RESOURCES: &str = "tembra:guest/resources@1.0.0";
const GUEST_RESOURCES_INIT: &str = "tembra:guest/resources-init@1.0.0";
const GUEST_CONTEXT: &str = "tembra:guest/context@1.0.0";

/// The kernel's `tembra:guest` package in nested form, appended to the inline WIT so the bindings
/// of a state resolve it; `cargo tembra` reads the same package from `tembra-contracts`.
pub(crate) fn guest_package() -> String {
    const GUEST: &str = include_str!("../../contracts/wit/tembra/guest.wit");
    let (package, body) = GUEST
        .split_once(";\n")
        .expect("the guest package starts with its declaration");
    format!("{package} {{\n{}}}\n", body.trim_start_matches('\n'))
}

/// A directory of consumed WIT packages, resolved against the crate manifest.
pub(crate) fn absolute(dir: &str) -> syn::Result<PathBuf> {
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR")
        .ok_or_else(|| error("CARGO_MANIFEST_DIR is not set"))?;
    Ok(PathBuf::from(manifest).join(dir))
}

/// The callback of one hook, read from the consumed packages: its name, which compositions its
/// shape admits, and how the bindings spell its input and result.
pub(crate) struct Callback {
    pub name: String,
    /// The callback returns nothing.
    pub series: bool,
    /// The callback returns its input type.
    pub waterfall: bool,
    /// The callback returns an option.
    pub bail: bool,
    pub input: Spelled,
    pub result: Option<Spelled>,
}

/// A WIT type as the generated bindings spell it in the hook's import module and in its export
/// module, which are distinct Rust types.
pub(crate) struct Spelled {
    pub import: String,
    pub export: String,
}

/// The callback of every hook: the single function of its interface in the consumed packages.
pub(crate) fn callbacks<'a>(
    dir: &Path,
    hooks: impl Iterator<Item = &'a String>,
) -> syn::Result<BTreeMap<String, Callback>> {
    let mut resolve = Resolve::default();
    resolve.push_dir(dir).map_err(|cause| {
        error(format!(
            "cannot read the WIT packages in {}: {cause:#}",
            dir.display()
        ))
    })?;
    hooks
        .map(|hook| {
            let (_, interface) = resolve
                .interfaces
                .iter()
                .find(|(id, _)| resolve.id_of(*id).as_deref() == Some(hook.as_str()))
                .ok_or_else(|| {
                    error(format!(
                        "hook {hook} is not an interface of the WIT packages in {}",
                        dir.display()
                    ))
                })?;
            let mut functions = interface.functions.values();
            let (Some(function), None) = (functions.next(), functions.next()) else {
                return Err(error(format!(
                    "hook {hook} must declare exactly one callback"
                )));
            };
            let [param] = function.params.as_slice() else {
                return Err(error(format!(
                    "the callback of hook {hook} takes exactly one input"
                )));
            };
            let [namespace, package, interface] = crate::hook_path(hook);
            let import = format!("self::{namespace}::{package}::{interface}");
            let export = format!("exports::{namespace}::{package}::{interface}");
            let spell = |ty: wit_parser::Type| -> syn::Result<Spelled> {
                Ok(Spelled {
                    import: rust_type(&resolve, ty, &import)?,
                    export: rust_type(&resolve, ty, &export)?,
                })
            };
            Ok((
                hook.clone(),
                Callback {
                    name: function.name.clone(),
                    series: function.result.is_none(),
                    waterfall: function.result == Some(param.ty),
                    bail: function.result.is_some_and(|ty| optional(&resolve, ty)),
                    input: spell(param.ty)?,
                    result: function.result.map(spell).transpose()?,
                },
            ))
        })
        .collect()
}

/// Whether a WIT type is an option, through aliases.
fn optional(resolve: &Resolve, mut ty: wit_parser::Type) -> bool {
    loop {
        let wit_parser::Type::Id(id) = ty else {
            return false;
        };
        match &resolve.types[id].kind {
            TypeDefKind::Option(_) => return true,
            TypeDefKind::Type(inner) => ty = *inner,
            _ => return false,
        }
    }
}

/// Spell a WIT type the way `wit-bindgen` names it in `module`, the generated module of its
/// interface: named types in UpperCamelCase, lists and strings from `alloc`, options, results and
/// tuples structurally.
fn rust_type(resolve: &Resolve, ty: wit_parser::Type, module: &str) -> syn::Result<String> {
    use wit_parser::Type;
    Ok(match ty {
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
        Type::String => "__tembra_alloc::string::String".into(),
        Type::ErrorContext => {
            return Err(error("a hook callback cannot carry an error context"));
        }
        Type::Id(id) => {
            let definition = &resolve.types[id];
            if let Some(name) = &definition.name {
                return Ok(format!("{module}::{}", name.to_upper_camel_case()));
            }
            match &definition.kind {
                TypeDefKind::Option(inner) => {
                    format!("Option<{}>", rust_type(resolve, *inner, module)?)
                }
                TypeDefKind::List(inner) => format!(
                    "__tembra_alloc::vec::Vec<{}>",
                    rust_type(resolve, *inner, module)?
                ),
                TypeDefKind::Result(result) => {
                    let side = |ty: Option<wit_parser::Type>| match ty {
                        Some(ty) => rust_type(resolve, ty, module),
                        None => Ok("()".to_owned()),
                    };
                    format!("Result<{}, {}>", side(result.ok)?, side(result.err)?)
                }
                TypeDefKind::Tuple(tuple) => {
                    let types = tuple
                        .types
                        .iter()
                        .map(|ty| rust_type(resolve, *ty, module))
                        .collect::<syn::Result<Vec<_>>>()?;
                    format!("({},)", types.join(", "))
                }
                TypeDefKind::Type(inner) => rust_type(resolve, *inner, module)?,
                _ => {
                    return Err(error(
                        "this type of the hook callback has no spelling the macro knows; use records, enums, variants, scalars, strings, lists, options, results and tuples",
                    ));
                }
            }
        }
    })
}

fn error(message: impl std::fmt::Display) -> syn::Error {
    syn::Error::new(Span::call_site(), message.to_string())
}

/// A struct with named fields becomes a `record`.
pub(crate) fn record(item: &ItemStruct) -> syn::Result<Shape> {
    let Fields::Named(fields) = &item.fields else {
        return Err(syn::Error::new(item.span(), "a record needs named fields"));
    };
    let mut wit = String::new();
    for doc in docs(&item.attrs) {
        wit.push_str(&format!("  ///{doc}\n"));
    }
    wit.push_str(&format!("  record {} {{\n", kebab(&item.ident)));
    let mut references = Vec::new();
    for field in &fields.named {
        let Some(ident) = &field.ident else {
            return Err(syn::Error::new(field.span(), "a record field needs a name"));
        };
        wit.push_str(&format!(
            "    {}: {},\n",
            kebab(ident),
            wit_type(&field.ty)?
        ));
        self::references(&field.ty, &mut references);
    }
    wit.push_str("  }\n");
    Ok(Shape {
        ident: item.ident.clone(),
        wit,
        references,
    })
}

/// An enum becomes an `enum` when every variant is empty, otherwise a `variant` with one payload
/// per case.
pub(crate) fn enumeration(item: &ItemEnum) -> syn::Result<Shape> {
    let plain = item
        .variants
        .iter()
        .all(|variant| matches!(variant.fields, Fields::Unit));
    let mut wit = String::new();
    for doc in docs(&item.attrs) {
        wit.push_str(&format!("  ///{doc}\n"));
    }
    let keyword = if plain { "enum" } else { "variant" };
    wit.push_str(&format!("  {keyword} {} {{\n", kebab(&item.ident)));
    let mut references = Vec::new();
    for variant in &item.variants {
        let case = kebab(&variant.ident);
        match &variant.fields {
            Fields::Unit => wit.push_str(&format!("    {case},\n")),
            Fields::Unnamed(fields) if fields.unnamed.len() == 1 => {
                wit.push_str(&format!(
                    "    {case}({}),\n",
                    wit_type(&fields.unnamed[0].ty)?
                ));
                self::references(&fields.unnamed[0].ty, &mut references);
            }
            _ => {
                return Err(syn::Error::new(
                    variant.span(),
                    "a variant carries at most one unnamed payload",
                ));
            }
        }
    }
    wit.push_str("  }\n");
    Ok(Shape {
        ident: item.ident.clone(),
        wit,
        references,
    })
}

/// Rust types with a WIT spelling of their own; any other name is a type of the module.
const BUILTIN: &[&str] = &[
    "i8", "i16", "i32", "i64", "u8", "u16", "u32", "u64", "f32", "f64", "bool", "char", "String",
    "Vec", "Option", "Result",
];

/// Collect the module types a Rust type mentions, looking through generics and tuples.
pub(crate) fn references(ty: &Type, out: &mut Vec<Ident>) {
    match ty {
        Type::Path(path) => {
            let Some(segment) = path.path.segments.last() else {
                return;
            };
            if !BUILTIN.contains(&segment.ident.to_string().as_str()) {
                out.push(segment.ident.clone());
            }
            if let syn::PathArguments::AngleBracketed(args) = &segment.arguments {
                for arg in &args.args {
                    if let syn::GenericArgument::Type(ty) = arg {
                        references(ty, out);
                    }
                }
            }
        }
        Type::Tuple(tuple) => {
            for element in &tuple.elems {
                references(element, out);
            }
        }
        _ => {}
    }
}

/// Translate an owned Rust type into its WIT spelling.
pub(crate) fn wit_type(ty: &Type) -> syn::Result<String> {
    match ty {
        Type::Path(path) => {
            let segment = path
                .path
                .segments
                .last()
                .ok_or_else(|| syn::Error::new(ty.span(), "empty type path"))?;
            let name = segment.ident.to_string();
            let args = || -> syn::Result<Vec<String>> {
                let syn::PathArguments::AngleBracketed(args) = &segment.arguments else {
                    return Err(syn::Error::new(
                        segment.span(),
                        "type parameters are required",
                    ));
                };
                args.args
                    .iter()
                    .map(|arg| match arg {
                        syn::GenericArgument::Type(Type::Tuple(t)) if t.elems.is_empty() => {
                            Ok("_".to_owned())
                        }
                        syn::GenericArgument::Type(ty) => wit_type(ty),
                        other => Err(syn::Error::new(other.span(), "unsupported type parameter")),
                    })
                    .collect()
            };
            Ok(match name.as_str() {
                "i8" => "s8".into(),
                "i16" => "s16".into(),
                "i32" => "s32".into(),
                "i64" => "s64".into(),
                "u8" | "u16" | "u32" | "u64" | "f32" | "f64" | "bool" | "char" => name,
                "String" => "string".into(),
                "Vec" => format!("list<{}>", args()?.join(", ")),
                "Option" => format!("option<{}>", args()?.join(", ")),
                "Result" => format!("result<{}>", args()?.join(", ")),
                other => other.to_kebab_case(),
            })
        }
        Type::Tuple(tuple) => Ok(format!(
            "tuple<{}>",
            tuple
                .elems
                .iter()
                .map(wit_type)
                .collect::<syn::Result<Vec<_>>>()?
                .join(", ")
        )),
        Type::Reference(_) => Err(syn::Error::new(
            ty.span(),
            "WIT boundaries pass owned values; use String, Vec<T>, ... without &",
        )),
        other => Err(syn::Error::new(
            other.span(),
            "this type has no WIT equivalent",
        )),
    }
}

/// The `///` lines of an item, carried into the WIT.
pub(crate) fn docs(attrs: &[Attribute]) -> Vec<String> {
    attrs
        .iter()
        .filter_map(|attr| match &attr.meta {
            Meta::NameValue(meta) if meta.path.is_ident("doc") => match &meta.value {
                Expr::Lit(lit) => match &lit.lit {
                    Lit::Str(s) => Some(s.value()),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        })
        .collect()
}
