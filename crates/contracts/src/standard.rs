//! Canonical standard contracts are library data, not files each Orb author must maintain.
use crate::Error;
use wit_parser::{PackageName, Resolve, UnresolvedPackage, UnresolvedPackageGroup};

/// The kernel's guest interfaces, bundled after the WASI release they build on.
const GUEST: (&str, &str) = ("tembra-guest.wit", include_str!("../wit/tembra/guest.wit"));

/// The bundled standard contracts in dependency order, each one package in one file: the
/// supported WASI release, then the kernel's guest interfaces. Each comes with its file name.
pub(crate) fn sources() -> impl Iterator<Item = (&'static str, &'static str)> {
    crate::WASI_SOURCES.iter().copied().chain([GUEST])
}

/// The bundled standard contracts with the name of the package each defines, in dependency order.
pub(crate) fn packages() -> Vec<(PackageName, &'static str, &'static str)> {
    sources()
        .map(|(file, source)| {
            let group = UnresolvedPackageGroup::parse(file, source)
                .unwrap_or_else(|_| panic!("the bundled standard contract {file} parses"));
            (group.main.name, file, source)
        })
        .collect()
}

/// Whether a WIT source is one of the bundled standard contracts, line for line, whichever line
/// endings a checkout gave it.
pub(crate) fn bundled(source: &str) -> bool {
    sources().any(|(_, bundled)| bundled.lines().eq(source.lines()))
}

/// Load the supported WASI release and the kernel's guest interfaces before resolving the
/// caller's package graph. The unresolved packages are kept so explicit imports through
/// `include wasi:...` keep their provenance.
pub(crate) fn resolve() -> Result<(Resolve, Vec<UnresolvedPackage>), Error> {
    let mut resolve = Resolve::default();
    let mut known = Vec::new();
    for (name, source) in sources() {
        let group = crate::authoring::parse_source(name, source)?;
        known.push(group.main.clone());
        known.extend(group.nested.iter().cloned());
        resolve
            .push_group(group)
            .map_err(|error| Error(error.to_string()))?;
    }
    Ok((resolve, known))
}
