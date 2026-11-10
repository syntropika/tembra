//! Recover explicit world imports before wit-parser elaboration erases their provenance.
//!
//! `Resolve::push*` elaborates every world: an interface reached only because another item
//! uses its types is inserted exactly like the world's own import statements (proof:
//! `PHASE3_IMPORT_PROOF.md`). Callable authority must follow the world's own imports, directly
//! or through `include`, so they are read from the public unresolved packages.
use std::collections::{BTreeMap, BTreeSet};
use wit_parser::{
    AstItem, InterfaceId, PackageName, Resolve, Stability, UnresolvedPackage, WorldId, WorldItem,
    WorldKey,
};

/// Includes deeper than this are refused by resolution anyway; stay bounded regardless.
const DEPTH: usize = 64;

/// Explicit import keys of the resolved `world`, spelled as `Resolve::name_world_key` spells
/// them. The world is found by its package-qualified identity among every parsed package
/// (`known`: bundled standard, `deps/`, main and nested; names are unique there), so worlds with
/// equal bare names in different packages never share provenance.
///
/// `None` means provenance is unknown: the world, or a world it includes, comes from an encoded
/// package without an unresolved form, so every import is treated as explicit.
pub(crate) fn world(
    resolve: &Resolve,
    world: WorldId,
    known: &[UnresolvedPackage],
) -> Option<BTreeSet<String>> {
    let selected = &resolve.worlds[world];
    let package = &resolve.packages[selected.package?].name;
    let (unresolved, id) = find(known, package, &selected.name)?;
    imports(resolve, unresolved, id, known, 0)
}

/// The unresolved declaration (not a foreign placeholder) of `package/world`.
fn find<'a>(
    known: &'a [UnresolvedPackage],
    package: &PackageName,
    world: &str,
) -> Option<(&'a UnresolvedPackage, WorldId)> {
    let unresolved = known.iter().find(|candidate| candidate.name == *package)?;
    let (id, _) = unresolved.worlds.iter().find(|(id, candidate)| {
        candidate.name == world && foreign_world(unresolved, *id).is_none()
    })?;
    Some((unresolved, id))
}

/// Whether resolution keeps an item with this stability; mirrors wit-parser's
/// `include_stability`. `@since` items that resolution accepted are active, and `@unstable`
/// items are active only when their feature is enabled on this `Resolve`.
fn active(resolve: &Resolve, stability: &Stability) -> bool {
    match stability {
        Stability::Unstable { feature, .. } => {
            resolve.all_features || resolve.features.contains(feature)
        }
        _ => true,
    }
}

fn imports(
    resolve: &Resolve,
    package: &UnresolvedPackage,
    world: WorldId,
    known: &[UnresolvedPackage],
    depth: usize,
) -> Option<BTreeSet<String>> {
    if depth > DEPTH {
        return None;
    }
    let world = &package.worlds[world];
    let mut explicit = BTreeSet::new();
    for (key, item) in &world.imports {
        let WorldItem::Interface { id, stability, .. } = item else {
            continue;
        };
        // A gated-off import is dropped by resolution; elaboration may still add the same
        // interface as a type-only dependency, which must stay non-authorizing.
        if !active(resolve, stability) {
            continue;
        }
        explicit.insert(match key {
            WorldKey::Name(name) => name.clone(),
            WorldKey::Interface(_) => interface(package, *id)?,
        });
    }
    for include in &world.includes {
        if !active(resolve, &include.stability) {
            continue;
        }
        let included = match foreign_world(package, include.id) {
            None => imports(resolve, package, include.id, known, depth + 1)?,
            Some((name, world)) => {
                let (dependency, id) = find(known, &name, &world)?;
                imports(resolve, dependency, id, known, depth + 1)?
            }
        };
        // `include w with { a as b }` renames plain-name imports; interface keys contain ':'.
        let renames: BTreeMap<&str, &str> = include
            .names
            .iter()
            .map(|name| (name.name.as_str(), name.as_.as_str()))
            .collect();
        explicit.extend(included.into_iter().map(|key| {
            renames
                .get(key.as_str())
                .map_or(key, |renamed| (*renamed).to_owned())
        }));
    }
    Some(explicit)
}

/// `ns:pkg/name@version` of a local or foreign (placeholder) interface.
fn interface(package: &UnresolvedPackage, id: InterfaceId) -> Option<String> {
    for (name, items) in &package.foreign_deps {
        for (item, (ast, _)) in items {
            if matches!(ast, AstItem::Interface(found) if *found == id) {
                return Some(qualify(name, item));
            }
        }
    }
    Some(qualify(
        &package.name,
        package.interfaces[id].name.as_deref()?,
    ))
}

/// The foreign package and world name of a world placeholder.
fn foreign_world(package: &UnresolvedPackage, id: WorldId) -> Option<(PackageName, String)> {
    package.foreign_deps.iter().find_map(|(name, items)| {
        items.iter().find_map(|(item, (ast, _))| {
            matches!(ast, AstItem::World(found) if *found == id)
                .then(|| (name.clone(), item.clone()))
        })
    })
}

fn qualify(package: &PackageName, item: &str) -> String {
    match &package.version {
        Some(version) => format!("{}:{}/{item}@{version}", package.namespace, package.name),
        None => format!("{}:{}/{item}", package.namespace, package.name),
    }
}
