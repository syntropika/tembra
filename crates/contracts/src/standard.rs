//! Canonical standard contracts are library data, not files each Orb author must maintain.
use crate::Error;
use wit_parser::Resolve;

/// Load the supported WASI release before resolving the caller's package graph.
pub(crate) fn resolve() -> Result<Resolve, Error> {
    let mut resolve = Resolve::default();
    for (name, source) in [
        ("wasi-clocks.wit", include_str!("../wit/wasi03/clocks.wit")),
        (
            "wasi-filesystem.wit",
            include_str!("../wit/wasi03/filesystem.wit"),
        ),
        (
            "wasi-sockets.wit",
            include_str!("../wit/wasi03/sockets.wit"),
        ),
    ] {
        resolve
            .push_str(name, source)
            .map_err(|error| Error(format!("{error:#}")))?;
    }
    Ok(resolve)
}
