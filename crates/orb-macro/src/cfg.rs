//! `#[cfg(..)]` on the items the macro reads into the WIT and into `orb(..)`: a `#[service]`
//! struct, an inherent `impl` block of one, a `pub fn` or the `#[create]` of such a block, a free
//! function marked `#[root]`, a function or a struct marked `#[hook]`, and a sub-module holding any
//! of these, a service's own module first. An attribute macro reads its input before the compiler
//! evaluates the `#[cfg]`s inside it, so the macro cannot tell whether such an item exists in the
//! configuration being built. Instead, it expands the module once per combination of their distinct
//! predicates, a predicate under `not(..)` being the same one: each copy of the module leaves out
//! the items its combination rules out, drops the `#[cfg]`s it evaluated, and goes through
//! `#[tembra::orb]` again under the `#[cfg]` of its combination, which the copy also carries for
//! the errors of its expansion. The compiler keeps the one copy whose combination holds and expands
//! only that one, so the WIT, the signatures, `orb(..)` and the contract the macro writes are those
//! of the configuration being built. Any other `#[cfg]` is left to the compiler, such as one on a
//! field of the `#[inputs]` struct, which `orb(..)` takes whole.
use proc_macro2::{Delimiter, TokenStream, TokenTree};
use quote::quote;
use syn::{Attribute, ImplItem, Item, ItemMod, Type, Visibility, spanned::Spanned};

/// The most distinct predicates a module combines: 16 copies of the module.
const MOST: usize = 4;

/// The attribute a copy of the module carries, with the predicate of its combination, for the
/// errors of its expansion; the expansion removes it.
const CONFIGURATION: &str = "__tembra_configuration";

/// The attribute a copy puts on a module whose `#[service]` struct its combination leaves out,
/// with the service's name, so that the expansion refuses a `#[resource]` left there, which would
/// otherwise turn into a resource of the root; the expansion removes it.
const GATED: &str = "__tembra_gated_service";

/// The copies of the module `item`, one per combination of the predicates of the `#[cfg]`s on the
/// items the macro reads, each under the `#[cfg]` of its combination and the macro's attribute
/// with its options, `attribute`, through the facade named `facade`. None for a module without such
/// a `#[cfg]`, which the macro expands at once, and for an item that is no module, which the
/// expansion reports. More than `MOST` distinct predicates are an error at the first `#[cfg]`
/// beyond them.
pub(crate) fn combinations(
    attribute: &TokenStream,
    item: &TokenStream,
    facade: &syn::Ident,
) -> syn::Result<Option<TokenStream>> {
    let Ok(mut module) = syn::parse2::<ItemMod>(item.clone()) else {
        return Ok(None);
    };
    // The distinct predicates, in the order written, by their tokens; the reading keeps every
    // item and attribute.
    let mut predicates: Vec<(String, TokenStream)> = Vec::new();
    let mut beyond = None;
    if let Some((_, items)) = &mut module.content {
        places(items, &mut |attrs| {
            for attribute in attrs.iter() {
                let Some((predicate, _)) = predicate(attribute) else {
                    continue;
                };
                let key = predicate.to_string();
                if predicates.iter().any(|(known, _)| *known == key) {
                    continue;
                }
                if predicates.len() == MOST {
                    beyond.get_or_insert(attribute.span());
                } else {
                    predicates.push((key, predicate));
                }
            }
            true
        });
    }
    if let Some(span) = beyond {
        return Err(syn::Error::new(
            span,
            format!(
                "more than {MOST} distinct #[cfg] predicates on the module's services, methods, root and hooks: the macro expands the module once per combination of them, and combines at most {MOST}; gate these items under fewer predicates, such as one feature"
            ),
        ));
    }
    if predicates.is_empty() {
        return Ok(None);
    }
    let marker = syn::Ident::new(CONFIGURATION, proc_macro2::Span::call_site());
    let copies = (0..1usize << predicates.len()).map(|combination| {
        // The predicates that hold in this combination, one bit each.
        let holds = |index: usize| combination & (1 << index) != 0;
        let guard: Vec<TokenStream> = predicates
            .iter()
            .enumerate()
            .map(|(index, (_, predicate))| {
                if holds(index) {
                    quote! { #predicate }
                } else {
                    quote! { not(#predicate) }
                }
            })
            .collect();
        let mut copy = module.clone();
        // Whether an item's `#[cfg]`s that the macro evaluates hold in this combination.
        let kept = |attrs: &[Attribute]| {
            attrs.iter().all(|attribute| match predicate(attribute) {
                Some((predicate, positive)) => {
                    let key = predicate.to_string();
                    predicates
                        .iter()
                        .position(|(known, _)| *known == key)
                        .is_none_or(|index| holds(index) == positive)
                }
                None => true,
            })
        };
        if let Some((_, items)) = &mut copy.content {
            mark_gated(items, &kept);
            places(items, &mut |attrs| {
                let mut stays = true;
                attrs.retain(|attribute| match predicate(attribute) {
                    Some((predicate, positive)) => {
                        let key = predicate.to_string();
                        stays &= predicates
                            .iter()
                            .position(|(known, _)| *known == key)
                            .is_some_and(|index| holds(index) == positive);
                        false
                    }
                    None => true,
                });
                stays
            });
        }
        quote! {
            #[cfg(all(#(#guard),*))]
            #[::#facade::orb(#attribute)]
            #[#marker(all(#(#guard),*))]
            #copy
        }
    });
    Ok(Some(quote! { #(#copies)* }))
}

/// Mark each module among `items`, at any depth, whose `#[service]` struct `kept` leaves out with
/// `GATED` and the service's name.
fn mark_gated(items: &mut [Item], kept: &dyn Fn(&[Attribute]) -> bool) {
    for item in items {
        let Item::Mod(module) = item else {
            continue;
        };
        let Some((_, inner)) = &mut module.content else {
            continue;
        };
        let gated = inner.iter().find_map(|item| match item {
            Item::Struct(item) if marked(&item.attrs, "service") && !kept(&item.attrs) => {
                Some(item.ident.clone())
            }
            _ => None,
        });
        if let Some(service) = gated {
            let marker = syn::Ident::new(GATED, proc_macro2::Span::call_site());
            module
                .attrs
                .push(syn::parse_quote! { #[#marker(#service)] });
        }
        mark_gated(inner, kept);
    }
}

/// Take from a module's attributes the name of the service its copy's combination leaves out of
/// it, if any (`mark_gated`).
pub(crate) fn gated(attrs: &mut Vec<Attribute>) -> Option<syn::Ident> {
    let index = attrs
        .iter()
        .position(|attribute| attribute.path().is_ident(GATED))?;
    attrs.remove(index).parse_args().ok()
}

/// Take from a module's attributes the predicate of the combination it is the copy for, if it is
/// one: its expansion's errors name it.
pub(crate) fn configuration(attrs: &mut Vec<Attribute>) -> Option<TokenStream> {
    let index = attrs
        .iter()
        .position(|attribute| attribute.path().is_ident(CONFIGURATION))?;
    match attrs.remove(index).meta {
        syn::Meta::List(list) => Some(list.tokens),
        _ => None,
    }
}

/// An error of the expansion that a copy's `#[cfg]`s may cause, naming the copy's combination,
/// `configuration`, when the module is one: such as nothing to export, once they leave out every
/// service of the module.
pub(crate) fn under(configuration: Option<&TokenStream>, message: &str) -> String {
    match configuration {
        Some(configuration) => format!(
            "{message}; the module's #[cfg]s leave none under #[cfg({configuration})], the configuration of this build"
        ),
        None => message.to_owned(),
    }
}

/// Hand `place` the attributes of each item the macro reads, in `items` and in their inline
/// sub-modules: a `#[service]` struct, an inherent `impl` block of a service of the same module,
/// a `pub fn` or the `#[create]` of such a block, a free function marked `#[root]`, a function or
/// a struct marked `#[hook]`, and a sub-module that holds one of these, at any depth. `place` may
/// remove attributes, and says whether the item stays; a block or a sub-module that does not stay
/// takes its items with it.
fn places(items: &mut Vec<Item>, place: &mut dyn FnMut(&mut Vec<Attribute>) -> bool) {
    // A service's methods are read from the impl blocks of its own module.
    let services: Vec<syn::Ident> = items
        .iter()
        .filter_map(|item| match item {
            Item::Struct(item) if marked(&item.attrs, "service") => Some(item.ident.clone()),
            _ => None,
        })
        .collect();
    items.retain_mut(|item| match item {
        Item::Struct(item) if marked(&item.attrs, "service") || marked(&item.attrs, "hook") => {
            place(&mut item.attrs)
        }
        Item::Fn(function)
            if marked(&function.attrs, "root") || marked(&function.attrs, "hook") =>
        {
            place(&mut function.attrs)
        }
        Item::Impl(block) if block.trait_.is_none() && serves(&block.self_ty, &services) => {
            if !place(&mut block.attrs) {
                return false;
            }
            block.items.retain_mut(|member| match member {
                ImplItem::Fn(function)
                    if matches!(function.vis, Visibility::Public(_))
                        || marked(&function.attrs, "create") =>
                {
                    place(&mut function.attrs)
                }
                _ => true,
            });
            true
        }
        Item::Mod(module) => {
            let Some((_, items)) = &mut module.content else {
                return true;
            };
            if reads(items) && !place(&mut module.attrs) {
                return false;
            }
            places(items, place);
            true
        }
        _ => true,
    });
}

/// Whether `items`, or an inline sub-module of theirs at any depth, hold an item whose `#[cfg]`
/// the macro evaluates: a `#[service]` struct, a `#[root]` function or a `#[hook]` item. A
/// sub-module holding none, such as a `#[cfg(test)] mod tests`, keeps its `#[cfg]` for the
/// compiler.
fn reads(items: &[Item]) -> bool {
    items.iter().any(|item| match item {
        Item::Struct(item) => marked(&item.attrs, "service") || marked(&item.attrs, "hook"),
        Item::Fn(function) => marked(&function.attrs, "root") || marked(&function.attrs, "hook"),
        Item::Mod(module) => module
            .content
            .as_ref()
            .is_some_and(|(_, items)| reads(items)),
        _ => false,
    })
}

/// Whether the attributes hold the macro's attribute `name`, `#[service]` for one.
fn marked(attrs: &[Attribute], name: &str) -> bool {
    attrs
        .iter()
        .any(|attribute| attribute.path().is_ident(name))
}

/// Whether an `impl` block's type is one of the module's services, named as the scan reads it.
fn serves(ty: &Type, services: &[syn::Ident]) -> bool {
    matches!(ty, Type::Path(path)
        if path.qself.is_none()
            && path.path.get_ident().is_some_and(|ident| services.contains(ident)))
}

/// The predicate of a `#[cfg(..)]`, as written, and whether the `#[cfg]` holds when it holds:
/// its one argument, less a trailing comma, out of each `not(..)` around it, each of which turns
/// the answer over. A malformed one is none: it stays on the item, where the compiler reports it.
fn predicate(attribute: &Attribute) -> Option<(TokenStream, bool)> {
    let syn::Meta::List(list) = &attribute.meta else {
        return None;
    };
    if !list.path.is_ident("cfg") {
        return None;
    }
    let mut predicate = single(list.tokens.clone())?;
    let mut positive = true;
    while let Some(negated) = negated(&predicate) {
        predicate = negated;
        positive = !positive;
    }
    Some((predicate, positive))
}

/// The predicate `not(..)` holds, if `predicate` is one with a single argument.
fn negated(predicate: &TokenStream) -> Option<TokenStream> {
    let mut tokens = predicate.clone().into_iter();
    match (tokens.next(), tokens.next(), tokens.next()) {
        (Some(TokenTree::Ident(not)), Some(TokenTree::Group(group)), None)
            if not == "not" && group.delimiter() == Delimiter::Parenthesis =>
        {
            single(group.stream())
        }
        _ => None,
    }
}

/// The one argument of a list of predicates, less a trailing comma; none for an empty list or a
/// list of several.
fn single(tokens: TokenStream) -> Option<TokenStream> {
    let mut arguments = vec![TokenStream::new()];
    for token in tokens {
        match &token {
            TokenTree::Punct(punct) if punct.as_char() == ',' => {
                arguments.push(TokenStream::new());
            }
            _ => arguments.last_mut()?.extend([token]),
        }
    }
    if arguments.len() > 1 && arguments.last().is_some_and(TokenStream::is_empty) {
        arguments.pop();
    }
    match <[TokenStream; 1]>::try_from(arguments) {
        Ok([argument]) if !argument.is_empty() => Some(argument),
        _ => None,
    }
}
