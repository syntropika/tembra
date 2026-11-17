//! Builds of Orb crates and of the facade that only Cargo sets up: features unified across a
//! workspace, the targets of a crate, the facade's features one at a time. Each test runs `cargo`
//! on a fixture of `config-fixtures/`, or on the facade, as a user's build would, with the lockfile
//! as committed. They share one target directory, `config-fixtures` in the workspace's, which is
//! kept between runs.
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
};

/// The manifest or the directory of a fixture.
fn fixture(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("config-fixtures")
        .join(path)
}

/// Run `cargo` with `args` on the manifest, in the shared target directory, and return its output
/// when it fails: the tests' report, then Cargo's errors.
fn cargo(manifest: &Path, args: &[&str]) -> Result<(), String> {
    // Beside the directory Cargo gives the tests, whose content CI's cache drops: the cache keeps
    // a target directory nested in the workspace's.
    let target = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .parent()
        .expect("the workspace's target directory")
        .join("config-fixtures");
    let output = Command::new(env!("CARGO"))
        .args(args)
        .arg("--locked")
        .arg("--manifest-path")
        .arg(manifest)
        .arg("--target-dir")
        .arg(target)
        .output()
        .unwrap();
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

/// Each file under `directory`, by its path there, with its text: none without the directory.
fn files(directory: &Path) -> BTreeMap<PathBuf, String> {
    let mut files = BTreeMap::new();
    if !directory.exists() {
        return files;
    }
    let mut pending = vec![directory.to_path_buf()];
    while let Some(path) = pending.pop() {
        for entry in std::fs::read_dir(&path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else {
                let text = std::fs::read_to_string(&path).unwrap();
                files.insert(path.strip_prefix(directory).unwrap().to_path_buf(), text);
            }
        }
    }
    files
}

/// A guest's `tembra` with only `macros` gets the facade's `host` when another member of its
/// workspace enables it, since Cargo unifies the features of the members it builds. The guest's
/// module, a hook declaration the native side does not cover, compiles all the same, so
/// `cargo check --workspace` passes.
#[test]
#[ignore = "runs cargo on a fixture: just test-configurations"]
fn a_workspace_whose_application_enables_host_checks_beside_a_guest() {
    let manifest = fixture("workspace/Cargo.toml");
    if let Err(errors) = cargo(&manifest, &["check", "--workspace"]) {
        panic!("{errors}");
    }
}

/// Only the library writes the crate's `contract/`. The Orb modules of an integration test, a
/// bench, a binary and an example built as one are not the library's: `cargo test` and
/// `cargo build --all-targets` leave the contract as the library wrote it, also with the test's
/// module in the library's package, which would remove the library's contract.
#[test]
#[ignore = "runs cargo on a fixture: just test-configurations"]
fn the_other_targets_of_a_library_leave_its_contract_unchanged() {
    let manifest = fixture("library/Cargo.toml");
    let contract = fixture("library/contract");
    // Each target compiles again, so each module expands and writes whatever it writes.
    cargo(&manifest, &["clean", "--package", "tembra-config-library"]).unwrap();
    if contract.exists() {
        std::fs::remove_dir_all(&contract).unwrap();
    }
    if let Err(errors) = cargo(&manifest, &["build", "--lib"]) {
        panic!("{errors}");
    }
    let written = files(&contract);
    assert_eq!(
        written.keys().collect::<Vec<_>>(),
        [Path::new("shelf.wit")],
        "the library writes its world"
    );
    for args in [&["test"][..], &["build", "--all-targets"]] {
        if let Err(errors) = cargo(&manifest, args) {
            panic!("cargo {args:?}\n{errors}");
        }
        assert_eq!(files(&contract), written, "after cargo {args:?}");
    }
}

/// The facade compiles natively with no feature and with each feature alone, so each feature
/// enables whatever its items need. `wasm` is left out: it brings Wasmtime, and the examples'
/// hosts build the facade with it alone.
#[test]
#[ignore = "runs cargo on the facade: just test-configurations"]
fn the_facade_checks_with_each_feature_alone() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../tembra/Cargo.toml");
    let features = [
        "",
        "host",
        "macros",
        "contracts",
        "authoring",
        "rpc",
        "hooks",
        "subscriptions",
        "views",
        "telemetry",
    ];
    let failed = features
        .into_iter()
        .filter_map(|feature| {
            cargo(
                &manifest,
                &["check", "--no-default-features", "--features", feature],
            )
            .err()
            .map(|errors| format!("--features '{feature}'\n{errors}"))
        })
        .collect::<Vec<_>>();
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

/// The facade compiles for wasm32 with `macros` alone, as a guest's build compiles it.
#[test]
#[ignore = "runs cargo on the facade: just test-configurations"]
fn the_facade_checks_for_wasm32_with_macros() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../tembra/Cargo.toml");
    let args = [
        "check",
        "--no-default-features",
        "--features",
        "macros",
        "--target",
        "wasm32-unknown-unknown",
    ];
    if let Err(errors) = cargo(&manifest, &args) {
        panic!("{errors}");
    }
}
