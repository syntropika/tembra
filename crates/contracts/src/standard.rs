//! Canonical standard contracts are library data, not files each Orb author must maintain.
use crate::Error;
use wit_parser::{Resolve, UnresolvedPackage};

/// Load the supported WASI release before resolving the caller's package graph. The unresolved
/// packages are kept so explicit imports through `include wasi:...` keep their provenance.
pub(crate) fn resolve() -> Result<(Resolve, Vec<UnresolvedPackage>), Error> {
    let mut resolve = Resolve::default();
    let mut known = Vec::new();
    for &(name, source) in crate::WASI_SOURCES {
        let group = crate::authoring::parse_source(name, source)?;
        known.push(group.main.clone());
        known.extend(group.nested.iter().cloned());
        resolve
            .push_group(group)
            .map_err(|error| Error(error.to_string()))?;
    }
    Ok((resolve, known))
}
