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
use crate::{
    Candidate, Declared, Elsewhere, Module, Shape, ShapeKind, Uses, identity, reaches_root,
    reaching, relative, wit,
};
use proc_macro2::Span;
use quote::quote;
use std::collections::BTreeMap;
use syn::{Attribute, Fields, Ident, Item, LitInt, LitStr, Type, spanned::Spanned};

/// The inherent functions the macro gives a flags struct, which the bindings call by these names.
const RESERVED: [&str; 3] = ["empty", "bits", "from_bits_retain"];

/// The most flags a WIT `flags` holds, as the component model rules.
const MOST: usize = 32;

/// Read a struct marked `#[flags]`, declared `depth` modules below the Orb module: named fields,
/// all `bool`, between one and 32, each the WIT name of a flag, no generic parameter; in a
/// sub-module, visible to the Orb module, whose bindings name it.
pub(crate) fn candidate(
    item: syn::ItemStruct,
    attribute: &Attribute,
    scope: usize,
    depth: usize,
) -> syn::Result<Candidate> {
    let ident = &item.ident;
    if !matches!(attribute.meta, syn::Meta::Path(_)) {
        return Err(syn::Error::new(
            attribute.span(),
            "#[flags] takes no options: each `bool` field of the struct is a flag",
        ));
    }
    if !reaches_root(&item.vis, depth) {
        return Err(syn::Error::new(
            ident.span(),
            format!(
                "`{ident}` is a flags of a sub-module: make it visible to the Orb module, {}, whose bindings name it",
                reaching(depth)
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
pub(crate) fn check_hooks(
    shapes: &[Shape],
    declared: &[Declared],
    modules: &crate::foreign::Modules,
) -> syn::Result<()> {
    let reached = wit::hook_shapes(shapes, declared, modules);
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

/// Refuse a `#[cfg(..)]`, written plainly, under `#[cfg_attr(..)]` or inside the module, on a
/// sub-module holding a flags struct that enters the WIT, in itself or below it: `with` names the
/// struct through every module of its path (`remapped`), in every configuration, also where the
/// attribute is off and the compiler drops the module, and the struct with it. The module nearest
/// the struct is checked first, as the scan checks one holding a struct the macro marks.
pub(crate) fn check_modules(shapes: &[Shape], tree: &Module) -> syn::Result<()> {
    for shape in shapes
        .iter()
        .filter(|shape| matches!(shape.kind, ShapeKind::Flags(_)))
    {
        for module in tree.around(shape.scope).into_iter().rev() {
            let Some(ident) = &module.ident else {
                continue;
            };
            crate::unconditional_marked(
                &module.attrs,
                &format!("module `{ident}` holding the flags"),
                &shape.ident,
                "maps the WIT flags onto it",
            )?;
        }
    }
    Ok(())
}

/// Refuse an impl under `#[cfg(..)]` that the macro left one of a flags struct's derives to
/// (`emit`), `Clone`, `Copy`, `Debug` or `serde`'s, where a record or a variant that enters the
/// WIT holds the struct, as the bindings make of them (`bindings`, by `crate::selector`): the
/// bindings need the trait of the struct in every configuration, as they implement `Debug` for
/// such a type and derive `Clone`, `Copy` and serde's traits on it as far as its contents allow
/// (`implemented`, `Generated::serialized`). A struct no such type holds needs none of them.
pub(crate) fn check_held(
    shapes: &[Shape],
    bindings: &wit::Bindings,
    placement: &BTreeMap<String, Ident>,
    package: &str,
    elsewhere: &BTreeMap<String, Elsewhere>,
) -> syn::Result<()> {
    for flags in shapes
        .iter()
        .filter(|shape| matches!(shape.kind, ShapeKind::Flags(_)))
    {
        let Some(besides) = elsewhere.get(&flags.ident.to_string()) else {
            continue;
        };
        let name = wit::kebab(&flags.ident);
        for holder in shapes {
            let selector = crate::selector(holder, placement, package);
            let Some(generated) = bindings.types.get(&selector) else {
                continue;
            };
            let holds = generated
                .parts
                .iter()
                .any(|part| matches!(&part.held, wit::Held::Own(held) if *held == name));
            if !holds {
                continue;
            }
            let own = crate::implemented(&holder.kind, generated);
            let needed = |trait_name: &str| match trait_name {
                "Clone" | "Copy" | "Debug" => own.iter().any(|(known, _)| *known == trait_name),
                "Serialize" | "Deserialize" => generated.serialized,
                _ => false,
            };
            if let Some((_, refusal)) = besides
                .conditional
                .iter()
                .find(|(trait_name, _)| needed(trait_name))
            {
                return Err(refusal.clone());
            }
        }
    }
    Ok(())
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
/// the functions the bindings call on it. A function of a reserved name in an impl block of the
/// struct, or a `BitOr` of the struct with itself or with a type parameter, written in any module
/// of the Orb, is refused: the macro's would clash with it, and so would a derive of the struct
/// implementing `BitOr`. The macro leaves one of its derives to the author's impls, which it reads
/// before the compiler evaluates `#[cfg(..)]`: those under it that leave a configuration without
/// `Clone` the struct's `Copy` needs are refused (`uncovered`). `uses` are the modules' imports,
/// through which the type of an impl is followed, and `declared` the types of the Orb's modules,
/// by scope, which tell a name a glob import brings in apart. Returns what each struct implements,
/// by its name: the macro's derives, the author's, and the traits the Orb's modules implement for
/// it by hand, which decide the derives of the generated types holding the struct, with the
/// refusal of the impls that give a trait in some configurations only, those the macro leaves its
/// derive to included.
pub(crate) fn emit(
    shapes: &[Shape],
    tree: &mut Module,
    modules: &[Vec<Ident>],
    impls: &[(usize, syn::ItemImpl)],
    uses: &[Uses],
    declared: &[(usize, Ident)],
    serde: bool,
) -> syn::Result<BTreeMap<String, Elsewhere>> {
    let mut traits = BTreeMap::new();
    for shape in shapes {
        let ShapeKind::Flags(item) = &shape.kind else {
            continue;
        };
        let mut item = (**item).clone();
        let ident = item.ident.clone();
        let names_struct = |ty: &Type, scope: usize| {
            names(ty, scope, &ident, shape.scope, modules, uses, declared)
        };
        let own = impls
            .iter()
            .filter(|(scope, block)| names_struct(&block.self_ty, *scope));
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
        let implemented = traits_implemented(tree, modules, names_struct);
        if let Some((_, block)) = implemented.get("BitOr").and_then(|hands| hands.last()) {
            return Err(syn::Error::new(
                block.span(),
                format!(
                    "`{ident}` is a #[flags] struct, for which the macro implements `BitOr`, as its bindings combine flags with `|`; remove this impl"
                ),
            ));
        }
        let bit_or = item
            .attrs
            .iter()
            .filter(|attribute| attribute.path().is_ident("derive"))
            .filter_map(|derive| {
                derive
                    .parse_args_with(
                        syn::punctuated::Punctuated::<syn::Path, syn::Token![,]>::parse_terminated,
                    )
                    .ok()
            })
            .flatten()
            .find(|path| {
                path.segments.last().is_some_and(|last| {
                    crate::implemented_by(&last.ident, &item.attrs)
                        .iter()
                        .any(|name| name == "BitOr")
                })
            });
        if let Some(path) = bit_or {
            return Err(syn::Error::new(
                path.span(),
                format!(
                    "`{}` on `{ident}`: `{ident}` is a #[flags] struct, for which the macro implements `BitOr`, as its bindings combine flags with `|`; remove the derive",
                    crate::written(&path)
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
        let mut has = Elsewhere::by_hand(&implemented, &ident, tree);
        let mut missing = Vec::new();
        for (name, path) in &wanted {
            if crate::derives(&item.attrs, name) {
                // The derive gives the trait in every configuration.
                has.conditional.remove(*name);
            } else if !implemented.contains_key(*name) {
                missing.push(path.clone());
            }
        }
        // `Copy` needs `Clone`, and the struct has `Copy` in every configuration unless the macro
        // leaves it to impls that give it in some only; a record holding the struct needs what
        // the bindings derive on it (`check_held`).
        let copy = !has.conditional.contains_key("Copy");
        if copy && let Some(refusal) = has.conditional.remove("Clone") {
            return Err(refusal);
        }
        has.traits.extend(
            wanted
                .iter()
                .map(|(name, _)| (*name).to_owned())
                .chain(crate::derived(&item.attrs)),
        );
        traits.insert(ident.to_string(), has);
        if !missing.is_empty() {
            item.attrs.push(syn::parse_quote!(#[derive(#(#missing),*)]));
        }
        let lowered = bits(&item);
        let module = tree.at(shape.scope);
        module.items.push(Item::Struct(item));
        module.generated.push(lowered);
    }
    Ok(traits)
}

/// The traits the Orb's modules implement by hand for the type `ident` of the module `home`, a
/// flags struct or not, by the last name of each trait's path, with the impls, as `emit` reads
/// them for a flags struct (`traits_implemented`): once the generated type replaces a record, a
/// variant or an enum, the author's impls apply to it, so the types holding it may derive what
/// they implement.
pub(crate) fn implemented_by_hand(
    tree: &mut Module,
    modules: &[Vec<Ident>],
    uses: &[Uses],
    declared: &[(usize, Ident)],
    ident: &Ident,
    home: usize,
) -> BTreeMap<String, Vec<(usize, syn::ItemImpl)>> {
    let names_type =
        |ty: &Type, scope: usize| names(ty, scope, ident, home, modules, uses, declared);
    traits_implemented(tree, modules, names_type)
}

/// The trait impls the Orb's modules write by hand for the type `ident` of the module `home`, a
/// flags struct or not, each with the module it is written in: those whose type, written in that
/// module, is the type (`names`).
pub(crate) fn impls_of(
    tree: &mut Module,
    modules: &[Vec<Ident>],
    uses: &[Uses],
    declared: &[(usize, Ident)],
    ident: &Ident,
    home: usize,
) -> Vec<(usize, syn::ItemImpl)> {
    trait_impls(tree, modules, |ty: &Type, scope: usize| {
        names(ty, scope, ident, home, modules, uses, declared)
    })
}

/// The trait impls the Orb's modules write, each with the module it is written in, whose type is
/// the one `names` tells, which says whether a type written in a module is it.
fn trait_impls(
    tree: &mut Module,
    modules: &[Vec<Ident>],
    names: impl Fn(&Type, usize) -> bool,
) -> Vec<(usize, syn::ItemImpl)> {
    let mut found = Vec::new();
    for scope in 0..modules.len() {
        for item in &tree.at(scope).items {
            if let Item::Impl(block) = item
                && block.trait_.is_some()
                && names(&block.self_ty, scope)
            {
                found.push((scope, block.clone()));
            }
        }
    }
    found
}

/// Whether a type, written in the module `scope`, is the type `ident` of the module `home`: a path
/// that names it in that module, followed through the module's `use`s, `self::`, `super::` and
/// the sub-modules (`type_module`), as `Access` in its own module, `model::Access` from the Orb
/// module and `Access` after `use super::model::Access;` in any module. A name the macro cannot
/// place, one a glob import brings into a module declaring no type of that name, is the type when
/// no other type of the Orb's modules has its name, as the state's `impl Default` is read; a path
/// leaving the Orb module is not the type.
fn names(
    ty: &Type,
    scope: usize,
    ident: &Ident,
    home: usize,
    modules: &[Vec<Ident>],
    uses: &[Uses],
    declared: &[(usize, Ident)],
) -> bool {
    let Type::Path(path) = ty else {
        return false;
    };
    if path.qself.is_some() {
        return false;
    }
    let Some((module, name)) = crate::type_module(&path.path, scope, uses, modules) else {
        return false;
    };
    if name != *ident {
        return false;
    }
    if module == home {
        return true;
    }
    let elsewhere = declared
        .iter()
        .any(|(scope, name)| *scope == module && *name == *ident);
    !elsewhere && declared.iter().filter(|(_, name)| *name == *ident).count() == 1
}

/// The traits the Orb's modules implement by hand for a flags struct, by the last name of each
/// trait's path, with the impls, each with the module it is written in; `names` tells whether a
/// type written in a module is the struct. `BitOr` counts where it overlaps the macro's own,
/// `BitOr<Self>`: written with no argument, with `Self`, with the struct, or with a type parameter
/// of the impl, which takes the struct too; one with another right-hand type does not.
fn traits_implemented(
    tree: &mut Module,
    modules: &[Vec<Ident>],
    names: impl Fn(&Type, usize) -> bool,
) -> BTreeMap<String, Vec<(usize, syn::ItemImpl)>> {
    let mut implemented: BTreeMap<String, Vec<(usize, syn::ItemImpl)>> = BTreeMap::new();
    for (scope, block) in trait_impls(tree, modules, &names) {
        let Some((_, path, _)) = &block.trait_ else {
            continue;
        };
        let Some(last) = path.segments.last() else {
            continue;
        };
        let name = last.ident.to_string();
        if last.ident == "BitOr" {
            let parameter = |rhs: &Type| {
                block.generics.type_params().any(|param| {
                    matches!(rhs, Type::Path(path)
                        if path.qself.is_none() && path.path.is_ident(&param.ident))
                })
            };
            let overlaps = match &last.arguments {
                syn::PathArguments::None => true,
                syn::PathArguments::AngleBracketed(arguments) => {
                    let mut arguments = arguments.args.iter();
                    match (arguments.next(), arguments.next()) {
                        (Some(syn::GenericArgument::Type(rhs)), None) => {
                            matches!(rhs, Type::Path(path)
                                if path.qself.is_none() && path.path.is_ident("Self"))
                                || names(rhs, scope)
                                || parameter(rhs)
                        }
                        _ => false,
                    }
                }
                syn::PathArguments::Parenthesized(_) => false,
            };
            if !overlaps {
                continue;
            }
        }
        implemented.entry(name).or_default().push((scope, block));
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
