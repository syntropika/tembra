//! The trait impls the Orb's modules write by hand for a type that enters the WIT, read before the
//! compiler resolves names: the impls whose type, written in its module, is the type.
use crate::{Module, foreign};
use std::collections::BTreeMap;
use syn::{Ident, Item, Type};

/// The traits the Orb's modules implement by hand for the type `ident` of the module `home`, by the
/// last name of each trait's path, with the impls: once the generated type replaces a record, a
/// variant or an enum, the author's impls apply to it, so the types holding it may derive what
/// they implement.
pub(crate) fn implemented_by_hand(
    tree: &mut Module,
    modules: &[Vec<Ident>],
    resolver: &foreign::Modules,
    ident: &Ident,
    home: usize,
) -> BTreeMap<String, Vec<(usize, syn::ItemImpl)>> {
    let mut implemented: BTreeMap<String, Vec<(usize, syn::ItemImpl)>> = BTreeMap::new();
    for (scope, block) in impls_of(tree, modules, resolver, ident, home) {
        let Some(last) = block
            .trait_
            .as_ref()
            .and_then(|(_, path, _)| path.segments.last())
        else {
            continue;
        };
        implemented
            .entry(last.ident.to_string())
            .or_default()
            .push((scope, block));
    }
    implemented
}

/// The trait impls the Orb's modules write by hand for the type `ident` of the module `home`, each
/// with the module it is written in: those whose type, written in that module, is the type
/// (`names`).
pub(crate) fn impls_of(
    tree: &mut Module,
    modules: &[Vec<Ident>],
    resolver: &foreign::Modules,
    ident: &Ident,
    home: usize,
) -> Vec<(usize, syn::ItemImpl)> {
    let mut found = Vec::new();
    for scope in 0..modules.len() {
        for item in &tree.at(scope).items {
            if let Item::Impl(block) = item
                && block.trait_.is_some()
                && names(&block.self_ty, scope, ident, home, resolver)
            {
                found.push((scope, block.clone()));
            }
        }
    }
    found
}

/// Whether a type, written in the module `scope`, is the type `ident` of the module `home`: a path
/// that resolves to it there as a contract type's name does, through the glob imports from the
/// Orb's modules too (`foreign::Modules::implemented_type`): `Access` in its own module,
/// `model::Access` from the Orb module, and `Access` after `use super::model::Access;`,
/// `use super::model::*;` or `type Access = super::model::Access;` in any module. A name the macro
/// does not place in the Orb, through `crate::` or out of a crate, is not the type.
fn names(ty: &Type, scope: usize, ident: &Ident, home: usize, resolver: &foreign::Modules) -> bool {
    resolver
        .implemented_type(ty, scope)
        .is_some_and(|(name, module)| name == *ident && module == home)
}
