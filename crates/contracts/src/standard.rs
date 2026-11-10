//! Canonical standard contracts are library data, not files each Orb author must maintain.
use crate::Error;
use wit_parser::{Resolve, UnresolvedPackage};

/// Load the supported WASI release and the kernel's guest interfaces before resolving the
/// caller's package graph. The unresolved packages are kept so explicit imports through
/// `include wasi:...` keep their provenance.
pub(crate) fn resolve() -> Result<(Resolve, Vec<UnresolvedPackage>), Error> {
    let mut resolve = Resolve::default();
    let mut known = Vec::new();
    for (name, source) in [
        ("wasi-clocks.wit", include_str!("../wit/wasi03/clocks.wit")),
        ("wasi-random.wit", include_str!("../wit/wasi03/random.wit")),
        (
            "wasi-filesystem.wit",
            include_str!("../wit/wasi03/filesystem.wit"),
        ),
        (
            "wasi-sockets.wit",
            include_str!("../wit/wasi03/sockets.wit"),
        ),
        ("wasi-cli.wit", include_str!("../wit/wasi03/cli.wit")),
        ("wasi-http.wit", include_str!("../wit/wasi03/http.wit")),
        ("tembra-guest.wit", include_str!("../wit/tembra/guest.wit")),
    ] {
        let group = crate::authoring::parse_source(name, source)?;
        known.push(group.main.clone());
        known.extend(group.nested.iter().cloned());
        resolve
            .push_group(group)
            .map_err(|error| Error(error.to_string()))?;
    }
    Ok((resolve, known))
}
