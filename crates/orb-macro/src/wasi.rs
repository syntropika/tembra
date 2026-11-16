//! The WASI interfaces an Orb imports. Tembra serves WASI 0.3, whose packages `tembra-contracts`
//! bundles, and nothing else under the `wasi` namespace: `import = "wasi:..."` names an interface
//! of those packages, which the macro finds in the bundled sources, never in the consumed
//! directory, and nests, with the packages it builds on, in the inline WIT the bindings read,
//! unless the consumed directory holds them already. Which grant an import needs is the
//! installation's rule, read from the same identity.
use crate::wit::{Copies, error};
use std::collections::BTreeSet;
use syn::LitStr;
use wit_parser::{InterfaceId, PackageId, Resolve, Type, TypeDefKind, TypeOwner, WorldItem};

/// Whether an imported interface is one of WASI, by its namespace, rather than another Orb's.
pub(crate) fn is_wasi(identity: &str) -> bool {
    identity.starts_with("wasi:")
}

#[cfg(test)]
thread_local! {
    /// How many times this thread has read the bundled WASI packages, for the test that an
    /// expansion reads them once.
    pub(crate) static BUNDLED_PARSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The bundled WASI packages, resolved from their sources in dependency order, and each package
/// with its source, in that order.
pub(crate) type Bundled = (Resolve, Vec<(PackageId, &'static str)>);

/// The bundled WASI packages, resolved, which `wit::Parsed` holds for an expansion.
pub(crate) fn bundled() -> syn::Result<Bundled> {
    #[cfg(test)]
    BUNDLED_PARSES.set(BUNDLED_PARSES.get() + 1);
    let mut resolve = Resolve::default();
    let mut sources = Vec::new();
    for (file, source) in tembra_contracts::WASI_SOURCES {
        let id = resolve
            .push_str(file, source)
            .map_err(|cause| error(format!("the bundled WASI package {file}: {cause:#}")))?;
        sources.push((id, *source));
    }
    Ok((resolve, sources))
}

/// The WASI packages the Orb's imports need, nested for the inline WIT: the package of each
/// imported WASI interface and the bundled packages it builds on, in dependency order, read from
/// the bundled packages `parsed` holds. A package the consumed directory holds a copy of already,
/// one of its bundled `copies` (`wit::bundled_copies`), is left out. An import in the `wasi`
/// namespace that names no interface of the bundled packages, another version or an unstable
/// interface, is an error on its literal, and so is an interface no host serves as an import, one
/// a component exports instead, and an import that needs what a copy decoded from an encoded file
/// lacks (`partial`).
pub(crate) fn packages(
    parsed: &crate::wit::Parsed,
    imports: &[LitStr],
    copies: &Copies,
) -> syn::Result<String> {
    let imported: Vec<&LitStr> = imports
        .iter()
        .filter(|import| is_wasi(&import.value()))
        .collect();
    if imported.is_empty() {
        return Ok(String::new());
    }
    let (bundled, sources) = parsed.bundled()?;
    let mut needed = BTreeSet::new();
    for import in imported {
        let identity = import.value();
        let interface = bundled
            .interfaces
            .iter()
            .find(|(id, _)| bundled.id_of(*id).as_deref() == Some(identity.as_str()));
        let Some((id, interface, package)) =
            interface.and_then(|(id, interface)| Some((id, interface, interface.package?)))
        else {
            return Err(syn::Error::new(
                import.span(),
                unknown(bundled, sources, &identity),
            ));
        };
        // The installation binds an import with functions to the host's provider of its grant,
        // and `tembra_contracts::host_serves` says which interfaces have one; an interface of
        // types only asks nothing of a host.
        if !interface.functions.is_empty() && !tembra_contracts::host_serves(&identity) {
            return Err(syn::Error::new(
                import.span(),
                unserved(bundled, package, &identity),
            ));
        }
        // The packages the import needs: its own and those it builds on.
        let mut wanted = BTreeSet::new();
        let mut pending = vec![package];
        while let Some(package) = pending.pop() {
            if wanted.insert(package) {
                pending.extend(bundled.package_direct_deps(package));
            }
        }
        if let Some(lack) = partial(parsed, bundled, copies, id, &wanted)? {
            return Err(syn::Error::new(import.span(), lack));
        }
        needed.extend(wanted);
    }
    let mut nested = String::new();
    for (id, source) in sources.iter().filter(|(id, _)| needed.contains(id)) {
        let name = &bundled.packages[*id].name;
        if copies.contains_key(name) {
            continue;
        }
        nested.push('\n');
        nested.push_str(&crate::wit::nested(source).ok_or_else(|| {
            error(format!(
                "the bundled WASI package {name} has no `package ..;` declaration"
            ))
        })?);
    }
    Ok(nested)
}

/// What the bindings would miss for an import in the consumed directory: a copy there of a bundled
/// package the import needs, decoded from encoded files of `deps/`, holds only the parts of the
/// package they use, and the bindings read the package from there. The imported interface must be
/// there whole, every type and function, and each package nested for the import, from the bundled
/// sources, must find there each interface its worlds name and each type its interfaces use. What
/// a copy lacks is named with the copy and the files it was decoded from.
fn partial(
    parsed: &crate::wit::Parsed,
    bundled: &Resolve,
    copies: &Copies,
    imported: InterfaceId,
    wanted: &BTreeSet<PackageId>,
) -> syn::Result<Option<String>> {
    // The encoded files a needed package's copy was decoded from, if it was decoded.
    let decoded = |package: PackageId| {
        copies
            .get(&bundled.packages[package].name)
            .filter(|files| !files.is_empty())
    };
    let Some(dir) = parsed
        .dir()
        .filter(|_| wanted.iter().any(|package| decoded(*package).is_some()))
    else {
        return Ok(None);
    };
    let consumed = dir.packages()?;
    // What the bindings read of the needed packages, in order: the imported interface, whole, then,
    // for each package nested, the interfaces its worlds name and the types its interfaces use,
    // each with the nested package that reads it.
    let whole = &bundled.interfaces[imported];
    let mut reads = vec![(
        imported,
        whole.types.keys().map(String::as_str).collect::<Vec<_>>(),
        whole
            .functions
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        None,
    )];
    for &package in wanted {
        if copies.contains_key(&bundled.packages[package].name) {
            continue;
        }
        let nested = &bundled.packages[package];
        for interface in nested.interfaces.values() {
            for ty in bundled.interfaces[*interface].types.values() {
                if let TypeDefKind::Type(Type::Id(used)) = bundled.types[*ty].kind
                    && let TypeOwner::Interface(owner) = bundled.types[used].owner
                {
                    let name = bundled.types[used].name.as_deref();
                    reads.push((owner, name.into_iter().collect(), Vec::new(), Some(package)));
                }
            }
        }
        for world in nested.worlds.values() {
            let world = &bundled.worlds[*world];
            for item in world.imports.values().chain(world.exports.values()) {
                if let WorldItem::Interface { id, .. } = item {
                    reads.push((*id, Vec::new(), Vec::new(), Some(package)));
                }
            }
        }
    }
    for (interface, types, functions, reader) in reads {
        let Some(package) = bundled.interfaces[interface].package else {
            continue;
        };
        let (Some(files), Some(identity)) = (decoded(package), bundled.id_of(interface)) else {
            continue;
        };
        let name = &bundled.packages[package].name;
        // The copy of the interface: the consumed interface of its name in the copy of its package.
        let copy = consumed
            .package_names
            .get(name)
            .and_then(|copy| {
                let interface = bundled.interfaces[interface].name.as_deref()?;
                consumed.packages[*copy].interfaces.get(interface)
            })
            .map(|copy| &consumed.interfaces[*copy]);
        let lack = match copy {
            None => identity,
            Some(copy) => {
                if let Some(ty) = types.iter().find(|ty| !copy.types.contains_key(**ty)) {
                    format!("the type {ty} of {identity}")
                } else if let Some(function) = functions
                    .iter()
                    .find(|function| !copy.functions.contains_key(**function))
                {
                    format!("the function {function} of {identity}")
                } else {
                    continue;
                }
            }
        };
        let files: Vec<String> = files
            .iter()
            .map(|file| file.display().to_string())
            .collect();
        let reader = reader
            .map(|reader| format!(", which {} uses", bundled.packages[reader].name))
            .unwrap_or_default();
        return Ok(Some(format!(
            "the consumed directory holds {name} only in part, decoded from {}, without {lack}{reader}; the bindings read {name} from there, so consume the contract as WIT files with {name} whole in `deps/`, as `cargo tembra build` writes it",
            files.join(", ")
        )));
    }
    Ok(None)
}

/// Why an identity of the `wasi` namespace names no bundled interface: the interfaces its package
/// has, when the package is bundled, else the bundled packages.
fn unknown(bundled: &Resolve, sources: &[(PackageId, &str)], identity: &str) -> String {
    let wanted = crate::Identity {
        interface: None,
        ..crate::Identity::of(identity)
    }
    .to_string();
    let names: Vec<String> = sources
        .iter()
        .map(|(id, _)| bundled.packages[*id].name.to_string())
        .collect();
    match sources
        .iter()
        .zip(&names)
        .find(|(_, name)| **name == wanted)
    {
        Some(((id, _), name)) => {
            let interfaces: Vec<&str> = bundled.packages[*id]
                .interfaces
                .keys()
                .map(String::as_str)
                .collect();
            format!(
                "{identity} is not an interface of {name}, which has {}",
                interfaces.join(", ")
            )
        }
        None => format!(
            "{identity} is not an interface of the WASI packages Tembra serves: {}",
            names.join(", ")
        ),
    }
}

/// Why an interface of a bundled package is not imported: no host serves it as an import, since
/// a component exports it instead; the interfaces of its package the hosts do serve, those with
/// a grant and those of types only.
fn unserved(bundled: &Resolve, package: PackageId, identity: &str) -> String {
    let package = &bundled.packages[package];
    let served: Vec<&str> = package
        .interfaces
        .iter()
        .filter(|(_, id)| {
            let interface = &bundled.interfaces[**id];
            interface.functions.is_empty()
                || bundled
                    .id_of(**id)
                    .is_some_and(|identity| tembra_contracts::host_serves(&identity))
        })
        .map(|(name, _)| name.as_str())
        .collect();
    format!(
        "no host serves {identity} as an import: a component exports it instead; of {}, the hosts serve {}",
        package.name,
        served.join(", ")
    )
}
