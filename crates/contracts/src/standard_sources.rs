//! Canonical sources of the bundled standard contracts, shared by authoring, host binding
//! generation and the Orb macro, which needs no WIT resolution of its own from this crate.

/// The bundled standard contracts, in dependency order, for build-time tooling.
#[doc(hidden)]
pub const WASI_SOURCES: &[(&str, &str)] = &[
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
];

/// The kernel's guest interfaces, bundled after the WASI release they build on.
const GUEST: (&str, &str) = ("tembra-guest.wit", include_str!("../wit/tembra/guest.wit"));

/// The bundled standard contracts in dependency order, each one package in one file: the
/// supported WASI release, then the kernel's guest interfaces. Each comes with its file name.
pub(crate) fn sources() -> impl Iterator<Item = (&'static str, &'static str)> {
    WASI_SOURCES.iter().copied().chain([GUEST])
}

/// Whether a WIT source is one of the bundled standard contracts, line for line, whichever line
/// endings a checkout gave it: the only copy of a bundled package `Contract::from_dir` reads, so
/// build-time tooling reading a copy follows the same rule.
#[doc(hidden)]
pub fn bundled(source: &str) -> bool {
    sources().any(|(_, bundled)| bundled.lines().eq(source.lines()))
}
