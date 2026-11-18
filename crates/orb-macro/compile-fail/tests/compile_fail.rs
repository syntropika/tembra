//! Each file under `tests/ui` is an Orb the macro or rustc refuses; its `.stderr` is the error,
//! on the author's line. To add a case, write `tests/ui/<name>.rs`, run
//! `TRYBUILD=overwrite cargo test -p tembra-orb-macro-compile-fail`, read the new `.stderr`, and
//! commit both. The `.stderr` files follow the toolchain of `rust-toolchain.toml`.
//!
//! trybuild builds each case in a project of its own, `target/tests/trybuild/<this crate>`, whose
//! `CARGO_MANIFEST_DIR` the macro reads a `wit = ".."` directory from: a case consuming a WIT
//! package names this crate's `wit/` from there, `../../../../crates/orb-macro/compile-fail/wit/..`,
//! which holds while the target directory is the workspace's `target/`.

#[test]
fn the_macros_errors_fall_on_the_authors_line() {
    trybuild::TestCases::new().compile_fail("tests/ui/*.rs");
}
