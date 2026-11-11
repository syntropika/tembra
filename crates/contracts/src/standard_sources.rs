//! Canonical WASI sources shared by authoring and host binding generation.

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
