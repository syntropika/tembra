//! The WASI interfaces an Orb imports. Tembra serves WASI 0.3, whose packages `tembra-contracts`
//! bundles, and nothing else under the `wasi` namespace: `import = "wasi:..."` names an interface
//! of those packages, which the macro finds in the bundled sources, never in the consumed
//! directory, and nests, with the packages it builds on, in the inline WIT the bindings read,
//! unless the consumed directory holds them already. Which grant an import needs is the
//! installation's rule, read from the same identity.
use crate::wit::error;
use std::{collections::BTreeSet, path::Path};
use syn::LitStr;
use wit_parser::{PackageId, Resolve};

/// Whether an imported interface is one of WASI, by its namespace, rather than another Orb's.
pub(crate) fn is_wasi(identity: &str) -> bool {
    identity.starts_with("wasi:")
}

/// The bundled WASI packages, resolved from their sources in dependency order, and each package
/// with its source, in that order.
fn bundled() -> syn::Result<(Resolve, Vec<(PackageId, &'static str)>)> {
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

/// The bundled WASI packages, resolved, for what an interface of them defines.
pub(crate) fn resolve() -> syn::Result<Resolve> {
    Ok(bundled()?.0)
}

/// The WASI packages the Orb's imports need, nested for the inline WIT: the package of each
/// imported WASI interface and the bundled packages it builds on, in dependency order. A package
/// the consumed directory `dir` holds a copy of already is left out (`wit::bundled_copy`). An
/// import in the `wasi` namespace that names no interface of the bundled packages, another
/// version or an unstable interface, is an error on its literal, and so is an interface no host
/// serves as an import, one a component exports instead.
pub(crate) fn packages(imports: &[LitStr], dir: Option<&Path>) -> syn::Result<String> {
    let imported: Vec<&LitStr> = imports
        .iter()
        .filter(|import| is_wasi(&import.value()))
        .collect();
    if imported.is_empty() {
        return Ok(String::new());
    }
    let (bundled, sources) = bundled()?;
    let mut pending = Vec::new();
    for import in imported {
        let identity = import.value();
        let interface = bundled
            .interfaces
            .iter()
            .find(|(id, _)| bundled.id_of(*id).as_deref() == Some(identity.as_str()))
            .map(|(_, interface)| interface);
        let Some((interface, package)) =
            interface.and_then(|interface| Some((interface, interface.package?)))
        else {
            return Err(syn::Error::new(
                import.span(),
                unknown(&bundled, &sources, &identity),
            ));
        };
        // The installation binds an import with functions to the host's provider of its grant,
        // and `tembra_contracts::host_serves` says which interfaces have one; an interface of
        // types only asks nothing of a host.
        if !interface.functions.is_empty() && !tembra_contracts::host_serves(&identity) {
            return Err(syn::Error::new(
                import.span(),
                unserved(&bundled, package, &identity),
            ));
        }
        pending.push(package);
    }
    let mut needed = BTreeSet::new();
    while let Some(package) = pending.pop() {
        if needed.insert(package) {
            pending.extend(bundled.package_direct_deps(package));
        }
    }
    let consumed = dir.map(crate::wit::resolve).transpose()?;
    let mut nested = String::new();
    for (id, source) in sources.iter().filter(|(id, _)| needed.contains(id)) {
        let name = &bundled.packages[*id].name;
        if let Some((dir, consumed)) = dir.zip(consumed.as_ref())
            && crate::wit::bundled_copy(consumed, dir, name, "a WASI package Tembra serves")?
        {
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
