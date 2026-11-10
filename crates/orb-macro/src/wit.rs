//! The WIT text derived from the module items, and the consumed packages it refers to.
use crate::{Service, Shape};
use heck::ToKebabCase;
use proc_macro2::Span;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};
use syn::{
    Attribute, Expr, Fields, Ident, ItemEnum, ItemStruct, Lit, Meta, Type, spanned::Spanned,
};

/// WIT names are kebab-case; Rust identifiers map to them one by one.
pub(crate) fn kebab(ident: &Ident) -> String {
    ident.to_string().to_kebab_case()
}

/// Write the package: one interface per service, the types in the first interface and borrowed by
/// the others with `use`, then a world importing and exporting every contributed hook and
/// exporting every interface.
pub(crate) fn render(
    package: &str,
    world: &str,
    shapes: &[Shape],
    services: &[Service],
    hooks: &BTreeSet<String>,
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
    for hook in hooks {
        wit.push_str(&format!("  import {hook};\n  export {hook};\n"));
    }
    for service in services {
        wit.push_str(&format!("  export {};\n", kebab(&service.ident)));
    }
    wit.push_str("}\n");
    wit
}

/// A directory of consumed WIT packages, resolved against the crate manifest.
pub(crate) fn absolute(dir: &str) -> syn::Result<PathBuf> {
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR")
        .ok_or_else(|| error("CARGO_MANIFEST_DIR is not set"))?;
    Ok(PathBuf::from(manifest).join(dir))
}

/// The callback of every hook: the single function of its interface in the consumed packages.
pub(crate) fn callbacks(
    dir: &Path,
    hooks: &BTreeSet<String>,
) -> syn::Result<BTreeMap<String, String>> {
    let mut resolve = wit_parser::Resolve::default();
    resolve.push_dir(dir).map_err(|cause| {
        error(format!(
            "cannot read the WIT packages in {}: {cause:#}",
            dir.display()
        ))
    })?;
    hooks
        .iter()
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
            let mut functions = interface.functions.keys();
            match (functions.next(), functions.next()) {
                (Some(callback), None) => Ok((hook.clone(), callback.clone())),
                _ => Err(error(format!(
                    "hook {hook} must declare exactly one callback"
                ))),
            }
        })
        .collect()
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
