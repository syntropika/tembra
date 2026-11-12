//! WIT `flags` spelled in Rust: `#[flags]` on a struct of `bool` fields declares
//! `flags <name> { .. }`, one flag per field in declaration order, each named in kebab-case.
//!
//! Unlike a record, the struct is not replaced by the type the bindings generate, a bitflags type
//! whose API differs from fields. `wit-bindgen`'s `with` option maps the WIT type onto the
//! author's struct, so the bindings' functions, records, variants and containers hold the struct
//! itself, with no conversion anywhere. The macro gives the struct what the bindings' lowering and
//! lifting call on it, as `wit-bindgen` 0.63 writes them: `empty()`, `bits()`,
//! `from_bits_retain(bits)` and `|`. It also derives what a generated record or variant needs of
//! its fields, `Clone`, `Copy` and `Debug`, and serde's traits in an Orb with a state, each unless
//! the author derives or implements it.
//!
//! The bindings name the type through the path `with` gives, from the export module of each
//! interface that defines or uses it, four modules below the Orb module. A derived hook's types are
//! named from its import module too, one module higher, which one path cannot serve: a hook's input
//! holds no flags.
use crate::{Candidate, Declared, Module, Shape, ShapeKind, identity, reaches_root, relative, wit};
use proc_macro2::Span;
use quote::quote;
use std::collections::BTreeMap;
use syn::{Attribute, Fields, Ident, Item, LitInt, LitStr, Type, spanned::Spanned};

/// The inherent functions the macro gives a flags struct, which the bindings call by these names.
const RESERVED: [&str; 3] = ["empty", "bits", "from_bits_retain"];

/// The most flags a WIT `flags` holds, as the component model rules.
const MOST: usize = 32;

/// Read a struct marked `#[flags]`: named fields, all `bool`, between one and 32, each the WIT
/// name of a flag, no generic parameter; in a sub-module, visible to the Orb module, whose bindings
/// name it.
pub(crate) fn candidate(
    item: syn::ItemStruct,
    attribute: &Attribute,
    scope: usize,
    nested: bool,
) -> syn::Result<Candidate> {
    let ident = &item.ident;
    if !matches!(attribute.meta, syn::Meta::Path(_)) {
        return Err(syn::Error::new(
            attribute.span(),
            "#[flags] takes no options: each `bool` field of the struct is a flag",
        ));
    }
    if nested && !reaches_root(&item.vis) {
        return Err(syn::Error::new(
            ident.span(),
            format!(
                "`{ident}` is a flags of a sub-module: make it visible to the Orb module, `pub` or `pub(super)`, whose bindings name it"
            ),
        ));
    }
    if !item.generics.params.is_empty() {
        return Err(syn::Error::new(
            item.generics.span(),
            format!("`{ident}` is a flags: it takes no generic parameter"),
        ));
    }
    let Fields::Named(fields) = &item.fields else {
        return Err(syn::Error::new(
            ident.span(),
            format!("`{ident}` is a flags: a struct with named `bool` fields, one per flag"),
        ));
    };
    let mut names: BTreeMap<String, &Ident> = BTreeMap::new();
    for (index, field) in fields.named.iter().enumerate() {
        let Some(name) = &field.ident else {
            continue;
        };
        if index == MOST {
            return Err(syn::Error::new(
                name.span(),
                format!(
                    "`{ident}` has {} fields: a WIT flags holds at most {MOST} flags, as the component model rules",
                    fields.named.len()
                ),
            ));
        }
        if !boolean(&field.ty) {
            return Err(syn::Error::new(
                field.ty.span(),
                format!(
                    "the field `{ident}::{name}` is no `bool`: each field of a flags is a flag, a `bool`"
                ),
            ));
        }
        let flag = wit::kebab(name);
        if let Some(problem) = wit::name_problem(&flag) {
            return Err(syn::Error::new(
                name.span(),
                format!(
                    "the field `{ident}::{name}` has no WIT name for its flag: its kebab-case form `{flag}` {problem}"
                ),
            ));
        }
        if let Some(previous) = names.insert(flag.clone(), name) {
            return Err(syn::Error::new(
                name.span(),
                format!(
                    "the fields `{ident}::{previous}` and `{ident}::{name}` are both the flag `{flag}`; rename one of them"
                ),
            ));
        }
    }
    if names.is_empty() {
        return Err(syn::Error::new(
            ident.span(),
            format!("`{ident}` has no field: a WIT flags holds at least one flag"),
        ));
    }
    Ok(Candidate {
        ident: ident.clone(),
        item: Item::Struct(item),
        references: Vec::new(),
        scope,
        flags: true,
    })
}

/// Whether a type is `bool`, by its name, as `wit_type` reads it.
fn boolean(ty: &Type) -> bool {
    let Type::Path(path) = ty else {
        return false;
    };
    path.qself.is_none()
        && path.path.segments.last().is_some_and(|segment| {
            segment.ident == "bool" && matches!(segment.arguments, syn::PathArguments::None)
        })
}

/// The WIT definition of a flags struct, with its documentation: one flag per field, in order.
pub(crate) fn shape(item: &syn::ItemStruct) -> syn::Result<Shape> {
    let mut text = String::new();
    for doc in wit::docs(&item.attrs)? {
        text.push_str(&format!("  ///{doc}\n"));
    }
    text.push_str(&format!(
        "  flags {} {{\n",
        wit::escaped(&wit::kebab(&item.ident))
    ));
    for name in item.fields.iter().filter_map(|field| field.ident.as_ref()) {
        text.push_str(&format!("    {},\n", wit::escaped(&wit::kebab(name))));
    }
    text.push_str("  }\n");
    Ok(Shape {
        ident: item.ident.clone(),
        wit: text,
        references: Vec::new(),
        kind: ShapeKind::Flags(Box::new(item.clone())),
        scope: 0,
        carried: Default::default(),
    })
}

/// Refuse a flags a derived hook's input reaches: the bindings name the hook's types from its
/// import module and from its export module, at two depths, and `with` names the author's struct
/// through one path.
pub(crate) fn check_hooks(shapes: &[Shape], declared: &[Declared]) -> syn::Result<()> {
    let reached = wit::hook_shapes(shapes, declared);
    let flags = shapes.iter().find(|shape| {
        matches!(shape.kind, ShapeKind::Flags(_)) && reached.contains(&shape.ident.to_string())
    });
    match flags {
        Some(shape) => Err(syn::Error::new(
            shape.ident.span(),
            format!(
                "`{}` is a flags that a derived hook's input reaches: the bindings name a hook's types from its import and from its export, and the macro maps a flags onto the author's struct through one path, so a hook's input holds no flags",
                shape.ident
            ),
        )),
        None => Ok(()),
    }
}

/// The entries of `wit-bindgen`'s `with` option mapping each flags type onto the author's struct:
/// the type's full WIT name, `ns:name/interface@version/type`, and the struct's path from the
/// export modules, four modules below the Orb module.
pub(crate) fn remapped(
    shapes: &[Shape],
    placement: &BTreeMap<String, Ident>,
    package: &str,
    modules: &[Vec<Ident>],
) -> Vec<proc_macro2::TokenStream> {
    shapes
        .iter()
        .filter(|shape| matches!(shape.kind, ShapeKind::Flags(_)))
        .map(|shape| {
            let interface = wit::kebab(&placement[&shape.ident.to_string()]);
            let name = format!(
                "{}/{}",
                identity(package, &interface),
                wit::kebab(&shape.ident)
            );
            let name = LitStr::new(&name, Span::call_site());
            let path = relative(4, &modules[shape.scope], &shape.ident);
            quote! { #name: #path }
        })
        .collect()
}

/// Put each flags struct that enters the WIT back in its module, with the derives it lacks and
/// the functions the bindings call on it. A function of the struct's own of a reserved name, or
/// its own `BitOr`, is refused: the macro's would clash with it.
pub(crate) fn emit(
    shapes: &[Shape],
    tree: &mut Module,
    modules: &[Vec<Ident>],
    impls: &[(usize, syn::ItemImpl)],
    serde: bool,
) -> syn::Result<()> {
    for shape in shapes {
        let ShapeKind::Flags(item) = &shape.kind else {
            continue;
        };
        let mut item = (**item).clone();
        let ident = item.ident.clone();
        let own = impls.iter().filter(|(scope, block)| {
            *scope == shape.scope
                && matches!(&*block.self_ty, Type::Path(path) if path.path.is_ident(&ident))
        });
        for (_, block) in own {
            for function in block.items.iter().filter_map(|item| match item {
                syn::ImplItem::Fn(function) => Some(function),
                _ => None,
            }) {
                let name = &function.sig.ident;
                if RESERVED.iter().any(|reserved| name == reserved) {
                    return Err(syn::Error::new(
                        name.span(),
                        format!(
                            "`{ident}::{name}` takes a name the macro gives a #[flags] struct, whose bindings call `{}` on it; rename it",
                            RESERVED.join("`, `")
                        ),
                    ));
                }
            }
        }
        let module = tree.at(&modules[shape.scope]);
        let implemented = traits_implemented(&module.items, &ident);
        if let Some(span) = implemented.get("BitOr") {
            return Err(syn::Error::new(
                *span,
                format!(
                    "`{ident}` is a #[flags] struct, for which the macro implements `BitOr`, as its bindings combine flags with `|`; remove this impl"
                ),
            ));
        }
        let mut wanted = vec![
            ("Clone", quote! { ::core::clone::Clone }),
            ("Copy", quote! { ::core::marker::Copy }),
            ("Debug", quote! { ::core::fmt::Debug }),
        ];
        if serde {
            wanted.push(("Serialize", quote! { ::serde::Serialize }));
            wanted.push(("Deserialize", quote! { ::serde::Deserialize }));
        }
        let missing: Vec<proc_macro2::TokenStream> = wanted
            .into_iter()
            .filter(|(name, _)| {
                !crate::derives(&item.attrs, name) && !implemented.contains_key(*name)
            })
            .map(|(_, path)| path)
            .collect();
        if !missing.is_empty() {
            item.attrs.push(syn::parse_quote!(#[derive(#(#missing),*)]));
        }
        let lowered = bits(&item);
        module.items.push(Item::Struct(item));
        module.generated.push(lowered);
    }
    Ok(())
}

/// The traits a module's items implement by hand for the type `ident`, by the last name of each
/// trait's path, with where each impl is written.
fn traits_implemented(items: &[Item], ident: &Ident) -> BTreeMap<String, Span> {
    let mut implemented = BTreeMap::new();
    for item in items {
        let Item::Impl(block) = item else {
            continue;
        };
        let Some((_, path, _)) = &block.trait_ else {
            continue;
        };
        let names_type = matches!(&*block.self_ty, Type::Path(ty)
            if ty.qself.is_none() && ty.path.is_ident(ident));
        if let (true, Some(last)) = (names_type, path.segments.last()) {
            implemented.insert(last.ident.to_string(), block.span());
        }
    }
    implemented
}

/// What the bindings call on a flags struct: `empty()`, `bits()` and `from_bits_retain(bits)`,
/// the `n`th field the bit `n`, and `|`, the union of two values.
fn bits(item: &syn::ItemStruct) -> proc_macro2::TokenStream {
    let ident = &item.ident;
    let fields: Vec<&Ident> = item
        .fields
        .iter()
        .filter_map(|field| field.ident.as_ref())
        .collect();
    let set = fields.iter().enumerate().map(|(index, field)| match index {
        0 => quote! { (self.#field as u32) },
        _ => {
            let shift = LitInt::new(&index.to_string(), Span::call_site());
            quote! { ((self.#field as u32) << #shift) }
        }
    });
    let masks =
        (0..fields.len()).map(|index| LitInt::new(&(1u32 << index).to_string(), Span::call_site()));
    quote! {
        /// What the bindings call on the flags, as `wit-bindgen` lowers and lifts them: the
        /// `n`th field is the bit `n`.
        #[doc(hidden)]
        impl #ident {
            /// No flag set.
            #[doc(hidden)]
            pub const fn empty() -> Self {
                Self { #(#fields: false),* }
            }

            /// The flags as bits.
            #[doc(hidden)]
            pub const fn bits(&self) -> u32 {
                #(#set)|*
            }

            /// The flags of `bits`.
            #[doc(hidden)]
            pub const fn from_bits_retain(bits: u32) -> Self {
                Self { #(#fields: (bits & #masks) != 0),* }
            }
        }

        impl ::core::ops::BitOr for #ident {
            type Output = Self;

            /// The flags either value sets.
            fn bitor(self, other: Self) -> Self {
                Self { #(#fields: self.#fields | other.#fields),* }
            }
        }
    }
}
