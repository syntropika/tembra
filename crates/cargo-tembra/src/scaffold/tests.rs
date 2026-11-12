//! The generated guest uses the facade's macros without linking the host implementation, from a
//! checkout by path or from the repository by git.
use super::{BUILT_FROM, Facade, checkout_above, facade, manifest, without_verbatim_prefix};
use crate::scratch;
use std::{
    path::{Path, PathBuf},
    process::Command,
};

/// A commit as the build script records it: the full hash.
const COMMIT: &str = "b2189c1a7f3e4d5c6b7a8f9e0d1c2b3a4f5e6d7c";

/// The `tembra` dependency of the manifest written for `facade`, read back as TOML.
fn dependency(facade: &Facade) -> toml::Value {
    let manifest: toml::Value = toml::from_str(&manifest("greeter", facade)).unwrap();
    manifest["dependencies"]["tembra"].clone()
}

/// The facade only with its macros: no host runtime in a guest.
fn assert_macros_only(dependency: &toml::Value) {
    assert_eq!(dependency["default-features"].as_bool(), Some(false));
    assert_eq!(
        dependency["features"],
        toml::Value::Array(vec![toml::Value::String("macros".to_owned())])
    );
}

/// The facade by path in the checkout at `root`, as `facade` writes it.
fn by_path(root: &Path) -> Facade {
    let root = without_verbatim_prefix(root.canonicalize().unwrap());
    let facade_crate = root.join("crates").join("tembra");
    Facade::Path(facade_crate.to_str().unwrap().to_owned())
}

#[test]
fn the_facade_path_is_written_as_a_toml_string_without_host_features() {
    // An ordinary path reads as it did; quotes and backslashes are escaped and read back.
    let path = "/home/ada/tembra/crates/tembra";
    for path in [
        path,
        r#"/home/ada/a "quoted" \ dir\tembra/crates/tembra"#,
        r"C:\Users\ada\tembra\crates\tembra",
    ] {
        let tembra = dependency(&Facade::Path(path.to_owned()));
        assert_eq!(tembra["path"].as_str(), Some(path));
        assert_eq!(tembra.get("git"), None);
        assert_macros_only(&tembra);
    }
}

#[test]
fn the_facade_repository_and_commit_are_written_as_a_git_dependency() {
    let tembra = dependency(&Facade::Git {
        repository: "https://github.com/syntropika/tembra".to_owned(),
        rev: COMMIT.to_owned(),
    });
    assert_eq!(
        tembra["git"].as_str(),
        Some("https://github.com/syntropika/tembra")
    );
    assert_eq!(tembra["rev"].as_str(), Some(COMMIT));
    assert_eq!(tembra.get("path"), None);
    assert_macros_only(&tembra);
    // Both are TOML strings: quotes and backslashes are escaped and read back.
    let (repository, rev) = (r#"file:///C:/a "quoted" \ dir"#, r#"a"b\c"#);
    let tembra = dependency(&Facade::Git {
        repository: repository.to_owned(),
        rev: rev.to_owned(),
    });
    assert_eq!(tembra["git"].as_str(), Some(repository));
    assert_eq!(tembra["rev"].as_str(), Some(rev));
}

#[test]
fn a_checkout_is_found_from_any_directory_inside_it() {
    let dir = scratch("checkout-above");
    let checkout = dir.join("tembra");
    let orbs = checkout.join("examples").join("orbs");
    std::fs::create_dir_all(checkout.join("crates").join("tembra")).unwrap();
    std::fs::create_dir_all(&orbs).unwrap();
    std::fs::write(checkout.join("crates/tembra/Cargo.toml"), "").unwrap();
    assert_eq!(checkout_above(&checkout), Some(checkout.clone()));
    assert_eq!(checkout_above(&orbs), Some(checkout.clone()));
    // The checkout found is the facade by path, though the commit is known.
    assert_eq!(
        facade(None, checkout_above(&orbs), Some(COMMIT)).unwrap(),
        by_path(&checkout)
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn tembra_names_the_checkout_before_the_one_above() {
    let dir = scratch("tembra-option");
    let (named, above) = (dir.join("named"), dir.join("above"));
    std::fs::create_dir_all(&named).unwrap();
    std::fs::create_dir_all(&above).unwrap();
    assert_eq!(
        facade(Some(named.clone()), Some(above), Some(COMMIT)).unwrap(),
        by_path(&named)
    );
    // Without a checkout above and without a commit alike.
    assert_eq!(
        facade(Some(named.clone()), None, None).unwrap(),
        by_path(&named)
    );
    // A root that does not exist is named in the error.
    let missing = dir.join("missing");
    let error = facade(Some(missing.clone()), None, Some(COMMIT)).unwrap_err();
    assert!(
        error.to_string().contains(&missing.display().to_string()),
        "{error}"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn outside_a_checkout_the_facade_is_the_repository_at_the_commit_built_from() {
    assert_eq!(
        facade(None, None, Some(COMMIT)).unwrap(),
        Facade::Git {
            repository: "https://github.com/syntropika/tembra".to_owned(),
            rev: COMMIT.to_owned(),
        }
    );
    // Without that commit there is nothing to pin: the default branch may hold another macro.
    let error = facade(None, None, None).unwrap_err().to_string();
    assert!(error.contains("pass --tembra <path>"), "{error}");
}

#[test]
fn the_commit_built_from_is_the_head_of_the_checkout_the_build_ran_in() {
    // HEAD of the repository at the workspace root, when git can read it there; otherwise, as for
    // an archive of the sources, none.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let head = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success() && root.join(".git").exists())
        .map(|output| String::from_utf8(output.stdout).unwrap().trim().to_owned());
    assert_eq!(BUILT_FROM, head.unwrap_or_default());
}

#[test]
fn a_verbatim_drive_path_loses_its_prefix() {
    let stripped = |path: &str| without_verbatim_prefix(PathBuf::from(path));
    assert_eq!(
        stripped(r"\\?\C:\Users\ada\tembra"),
        PathBuf::from(r"C:\Users\ada\tembra")
    );
    // A verbatim UNC or volume path, and an ordinary path, stay as they are.
    for path in [
        r"\\?\UNC\server\share\tembra",
        r"\\?\Volume{8e1c}\tembra",
        "/home/ada/tembra",
    ] {
        assert_eq!(stripped(path), PathBuf::from(path));
    }
}
