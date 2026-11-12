//! The build's reading of its inputs and writing of its outputs, checked without compiling an Orb.
use super::build;
use std::path::PathBuf;

/// An empty directory for one test, under the system's temporary directory.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cargo-tembra-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
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
