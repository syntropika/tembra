//! Each file under `tests/ui` is an Orb the macro or rustc refuses; its `.stderr` is the error,
//! on the author's line. To add a case, write `tests/ui/<name>.rs`, run
//! `TRYBUILD=overwrite cargo test -p tembra-orb-macro-compile-fail`, read the new `.stderr`, and
//! commit both. The `.stderr` files follow the toolchain of `rust-toolchain.toml`, and name no
//! private path of a dependency, which its next release may move.
//!
//! trybuild builds each case in a project of its own, `tests/trybuild/<this crate>` in the target
//! directory, whose `CARGO_MANIFEST_DIR` the macro reads a `wit = ".."` directory from. The test
//! copies this crate's `wit/` there first, as `consumed/`, so a case consuming a WIT package names
//! it `wit = "consumed/<package>"`, wherever the target directory is. Complete inline WIT
//! disables implicit loading of the project's `wit/` directory.

use std::fs;
use std::path::{Path, PathBuf};

#[test]
fn the_macros_errors_fall_on_the_authors_line() {
    // Cargo puts `CARGO_TARGET_TMPDIR` in the target directory, which trybuild reads from
    // `cargo metadata`, `CARGO_TARGET_DIR` included.
    let target = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .parent()
        .expect("the temporary directory sits in the target directory");
    let project: PathBuf = [
        target,
        Path::new("tests"),
        Path::new("trybuild"),
        Path::new(env!("CARGO_PKG_NAME")),
    ]
    .iter()
    .collect();
    let wit = Path::new(env!("CARGO_MANIFEST_DIR")).join("wit");
    copy(&wit, &project.join("consumed"));
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/ui/*.rs");
    cases.pass("tests/pass/*.rs");
}

/// A dependency's private module, such as serde's `__private229`, takes its release in its name:
/// a `.stderr` naming one breaks on the next release.
#[test]
fn no_error_names_a_private_path_of_a_dependency() {
    let ui = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/ui");
    let entries =
        fs::read_dir(&ui).unwrap_or_else(|error| panic!("read {}: {error}", ui.display()));
    for entry in entries {
        let path = entry.expect("an entry of tests/ui").path();
        if path
            .extension()
            .is_some_and(|extension| extension == "stderr")
        {
            let text = fs::read_to_string(&path).expect("a .stderr reads as text");
            assert!(
                !text.contains("__private"),
                "{} names a private path",
                path.display()
            );
        }
    }
}

/// Copy the directory `from` to `to`, replacing what `to` holds.
fn copy(from: &Path, to: &Path) {
    if to.exists() {
        fs::remove_dir_all(to).unwrap_or_else(|error| panic!("remove {}: {error}", to.display()));
    }
    fs::create_dir_all(to).unwrap_or_else(|error| panic!("create {}: {error}", to.display()));
    let entries =
        fs::read_dir(from).unwrap_or_else(|error| panic!("read {}: {error}", from.display()));
    for entry in entries {
        let entry = entry.unwrap_or_else(|error| panic!("read {}: {error}", from.display()));
        let (source, target) = (entry.path(), to.join(entry.file_name()));
        if source.is_dir() {
            copy(&source, &target);
        } else {
            fs::copy(&source, &target)
                .unwrap_or_else(|error| panic!("copy {}: {error}", source.display()));
        }
    }
}
