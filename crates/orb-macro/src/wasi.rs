//! The WASI interfaces an Orb imports. Tembra serves WASI 0.3, whose packages `tembra-contracts`
//! bundles, and nothing else under the `wasi` namespace: `import = "wasi:..."` names an interface
//! of those packages, which the macro finds in the bundled sources, never in the consumed
//! directory, and nests, with the packages it builds on, in the inline WIT the bindings read,
//! unless the consumed directory holds them already. Which grant an import needs is the
//! installation's rule, read from the same identity.
use proc_macro2::Span;
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

/// The WASI packages the Orb's imports need, nested for the inline WIT: the package of each
/// imported WASI interface and the bundled packages it builds on, in dependency order. A package
/// the consumed directory `dir` resolves already, as a contract `cargo tembra build` wrote does
/// from its `deps/`, is left out, since the bindings would read it twice; that copy must define
/// what the bundled package defines, whatever its comments, or the error names its file. An
/// import in the `wasi` namespace that names no interface of the bundled packages, another
/// version or an unstable interface, is an error on its literal.
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
        let package = bundled
            .interfaces
            .iter()
            .find(|(id, _)| bundled.id_of(*id).as_deref() == Some(identity.as_str()))
            .and_then(|(_, interface)| interface.package);
        match package {
            Some(package) => pending.push(package),
            None => {
                return Err(syn::Error::new(
                    import.span(),
                    unknown(&bundled, &sources, &identity),
                ));
            }
        }
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
        if let Some((consumed, &copy)) = consumed
            .as_ref()
            .and_then(|consumed| Some((consumed, consumed.package_names.get(name)?)))
        {
            if crate::wit::definitions(consumed, copy) != crate::wit::definitions(&bundled, *id) {
                let place = consumed.packages[copy]
                    .interfaces
                    .values()
                    .next()
                    .map(|interface| {
                        consumed
                            .source_map
                            .render_location(consumed.interfaces[*interface].span)
                    })
                    .or_else(|| dir.map(|dir| dir.display().to_string()))
                    .unwrap_or_default();
                return Err(error(format!(
                    "{place}: this copy of {name}, a WASI package Tembra serves, defines it otherwise than the one bundled with this macro; build the consumed contract with the `cargo tembra` of the same version"
                )));
            }
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
    let (package, rest) = identity.split_once('/').unwrap_or((identity, ""));
    let version = rest.split_once('@').map_or("", |(_, version)| version);
    let wanted = format!("{package}@{version}");
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

fn error(message: impl std::fmt::Display) -> syn::Error {
    syn::Error::new(Span::call_site(), message.to_string())
}
