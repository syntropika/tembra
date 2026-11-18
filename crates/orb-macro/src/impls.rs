//! The trait impls the Orb's modules write by hand for a type that enters the WIT, read before the
//! compiler resolves names: the impls whose type, written in its module, is the type.
use crate::{DeclaredTypes, Module, Uses};
use std::collections::BTreeMap;
use syn::{Ident, Item, Type};

/// The traits the Orb's modules implement by hand for the type `ident` of the module `home`, by the
/// last name of each trait's path, with the impls: once the generated type replaces a record, a
/// variant or an enum, the author's impls apply to it, so the types holding it may derive what
/// they implement.
pub(crate) fn implemented_by_hand(
    tree: &mut Module,
    modules: &[Vec<Ident>],
    uses: &[Uses],
    declared: DeclaredTypes<'_>,
    ident: &Ident,
    home: usize,
) -> BTreeMap<String, Vec<(usize, syn::ItemImpl)>> {
    let mut implemented: BTreeMap<String, Vec<(usize, syn::ItemImpl)>> = BTreeMap::new();
    for (scope, block) in impls_of(tree, modules, uses, declared, ident, home) {
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
    uses: &[Uses],
    declared: DeclaredTypes<'_>,
    ident: &Ident,
    home: usize,
) -> Vec<(usize, syn::ItemImpl)> {
    let mut found = Vec::new();
    for scope in 0..modules.len() {
        for item in &tree.at(scope).items {
            if let Item::Impl(block) = item
                && block.trait_.is_some()
                && names(&block.self_ty, scope, ident, home, modules, uses, declared)
            {
                found.push((scope, block.clone()));
            }
        }
    }
    found
}

/// Whether a type, written in the module `scope`, is the type `ident` of the module `home`: a path
/// that names it in that module, followed through the module's `use`s, `self::`, `super::`, the
/// sub-modules and the glob imports from the Orb's modules (`type_module`), as `Access` in its own
/// module, `model::Access` from the Orb module and `Access` after `use super::model::Access;` or
/// `use super::model::*;` in any module. A name the macro cannot place, one a glob import from a
/// module it does not read brings into a module declaring no type of that name, or one a `use`
/// leads to through `crate::`, is the type when no other type of the Orb's modules that may enter
/// the WIT has its name, as the state's `impl Default` is read; `declared` are the types of the
/// Orb's modules, an alias of a namesake left out, which stands for the type it names as a `use`
/// does, with the candidates for the WIT among them. A path leaving the Orb module through a
/// crate is not the type.
fn names(
    ty: &Type,
    scope: usize,
    ident: &Ident,
    home: usize,
    modules: &[Vec<Ident>],
    uses: &[Uses],
    declared: DeclaredTypes<'_>,
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
        .all
        .iter()
        .any(|(scope, name)| *scope == module && *name == *ident);
    !elsewhere
        && declared
            .candidates
            .iter()
            .filter(|(_, name)| *name == *ident)
            .count()
            == 1
}
