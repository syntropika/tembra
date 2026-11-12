//! The build's reading of its inputs and writing of its outputs, checked without compiling an Orb.
use super::{build, built_component, merged_checksums};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// An empty directory for one test, under the system's temporary directory.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cargo-tembra-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A `compiler-artifact` message as cargo writes it, for a target of the given kinds.
fn artifact(manifest: &Path, kinds: &[&str], filenames: &[&str]) -> String {
    json!({
        "reason": "compiler-artifact",
        "package_id": "path+file:///orb#orb@0.1.0",
        "manifest_path": manifest,
        "target": {"kind": kinds, "crate_types": kinds, "name": "orb", "src_path": "/orb/src/lib.rs"},
        "features": [],
        "filenames": filenames,
        "executable": null,
        "fresh": true,
    })
    .to_string()
}

#[test]
fn the_component_is_the_wasm_file_cargo_built_for_the_package() {
    let dir = scratch("built-component");
    let (orb, dependency) = (dir.join("orb"), dir.join("dependency"));
    for crate_dir in [&orb, &dependency] {
        std::fs::create_dir_all(crate_dir).unwrap();
        std::fs::write(crate_dir.join("Cargo.toml"), "").unwrap();
    }
    let package = orb.join("Cargo.toml").canonicalize().unwrap();
    // The library's name is not the package's, and other packages build a cdylib too.
    let messages = [
        artifact(
            &dependency.join("Cargo.toml"),
            &["cdylib", "rlib"],
            &["/target/dependency.wasm", "/target/libdependency.rlib"],
        ),
        artifact(&package, &["custom-build"], &["/target/build-script-build"]),
        "a procedural macro's print".to_owned(),
        artifact(&package, &["cdylib"], &["/target/greeter_orb.wasm"]),
        json!({"reason": "build-finished", "success": true}).to_string(),
    ]
    .join("\n");
    assert_eq!(
        built_component(messages.as_bytes(), &package).unwrap(),
        Some(PathBuf::from("/target/greeter_orb.wasm"))
    );
    // A library that is no cdylib is no component, whatever lies in the target directory.
    let rlib = artifact(&package, &["lib"], &["/target/liborb.rlib"]);
    assert_eq!(built_component(rlib.as_bytes(), &package).unwrap(), None);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn checksums_keep_the_lines_of_the_files_a_build_did_not_write() {
    let digest = |bytes: &[u8]| format!("{:x}", Sha256::digest(bytes));
    // Another Orb's outputs, and this Orb's component from an earlier build.
    let previous = format!(
        "{counter}  counter.wasm\n{counter}  counter.orb.wasm\n{counter}  tally.wit\n{old}  greeter.wasm\n",
        counter = digest(b"counter"),
        old = digest(b"an earlier greeter"),
    );
    let written = [
        ("greeter.wasm".to_owned(), b"greeter".to_vec()),
        ("greeter.orb.wasm".to_owned(), b"greeter artifact".to_vec()),
        ("greeter.wit".to_owned(), b"greeter contract".to_vec()),
    ];
    assert_eq!(
        merged_checksums(&previous, &written),
        format!(
            "{counter}  counter.orb.wasm\n{counter}  counter.wasm\n{}  greeter.orb.wasm\n{}  greeter.wasm\n{}  greeter.wit\n{counter}  tally.wit\n",
            digest(b"greeter artifact"),
            digest(b"greeter"),
            digest(b"greeter contract"),
            counter = digest(b"counter"),
        )
    );
    // In a new directory, the lines of this build alone.
    assert_eq!(
        merged_checksums("", &written[..1]),
        format!("{}  greeter.wasm\n", digest(b"greeter"))
    );
}

#[test]
fn a_manifest_without_a_package_name_is_refused_with_an_error() {
    let dir = scratch("package-name");
    for manifest in [
        "[workspace]\nmembers = [\"counter\"]\n",
        "[package]\nversion = \"0.1.0\"\n",
    ] {
        std::fs::write(dir.join("Cargo.toml"), manifest).unwrap();
        let error = build(Some(dir.join("Cargo.toml")), true, None).unwrap_err();
        assert_eq!(error.to_string(), "Cargo.toml has no package.name");
    }
    std::fs::remove_dir_all(dir).unwrap();
}
