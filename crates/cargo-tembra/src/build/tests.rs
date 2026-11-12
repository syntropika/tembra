//! The build's reading of its inputs and writing of its outputs, checked without compiling an Orb.
use super::{
    build, built_component, former_contracts, merged_checksums, resolves_alone, write_dependencies,
};
use crate::scratch;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tembra_contracts::Contract;
use wit_parser::Resolve;

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
fn a_former_contract_is_a_wit_file_of_the_same_package() {
    let dir = scratch("former-contracts");
    let contract = |package: &str, world: &str| {
        format!(
            "package {package};\n\ninterface counter {{\n  add: func(by: s64) -> s64;\n}}\n\n\
             world {world} {{\n  export counter;\n}}\n"
        )
    };
    let text = contract("example:counter@1.0.0", "ledger");
    let files = [
        // The file being written, as an earlier build left it.
        ("ledger.wit", text.clone()),
        // The world's former name: the one to remove.
        ("tally.wit", contract("example:counter@1.0.0", "tally")),
        // Another package, another version of this one, and this one nested in another.
        ("other.wit", contract("example:other@1.0.0", "other")),
        ("next.wit", contract("example:counter@2.0.0", "next")),
        (
            "speller.wit",
            "package example:speller@1.0.0;\n\nworld speller {\n  import example:counter/counter@1.0.0;\n}\n\n\
             package example:counter@1.0.0 {\n  interface counter {\n    add: func(by: s64) -> s64;\n  }\n}\n"
                .to_owned(),
        ),
        // A file that does not parse, and one that is no .wit file.
        ("broken.wit", "package example:counter@1.0.0;\n\nworld {\n".to_owned()),
        ("notes.txt", contract("example:counter@1.0.0", "notes")),
    ];
    for (name, content) in &files {
        std::fs::write(dir.join(name), content).unwrap();
    }
    let formers = former_contracts(&dir, &dir.join("ledger.wit"), &text).unwrap();
    assert_eq!(formers, [dir.join("tally.wit")]);
    std::fs::remove_dir_all(dir).unwrap();
}

/// The contract of an Orb whose world imports the kernel's state.
const TALLY: &str = "package example:first@1.0.0;

interface tally {
  bump: func() -> u32;
}

world first {
  import tembra:guest/state@1.0.0;
  export tembra:guest/state-init@1.0.0;
  export tally;
}
";

#[test]
fn the_contract_directory_holds_the_standard_packages_it_depends_on() {
    let dir = scratch("contract-dependencies");
    let (file, deps) = (dir.join("first.wit"), dir.join("deps"));
    let contract =
        Contract::from_source(&file.display().to_string(), TALLY, Some("first")).unwrap();
    let dependencies = contract.standard_dependencies();
    // Alone, the contract names a package the directory does not hold.
    let error = resolves_alone(&file, TALLY, &deps, &[]).unwrap_err();
    assert!(error.to_string().contains("tembra:guest@1.0.0"), "{error}");
    resolves_alone(&file, TALLY, &deps, &dependencies).unwrap();
    // A standard package an earlier build wrote and the contract no longer depends on is removed;
    // a file of another package stays.
    std::fs::create_dir_all(&deps).unwrap();
    let former = deps.join("wasi-random.wit");
    let notes = deps.join("notes.wit");
    std::fs::write(
        &former,
        "package wasi:random@0.3.0;\n\ninterface random {\n  get-random-u64: func() -> u64;\n}\n",
    )
    .unwrap();
    std::fs::write(
        &notes,
        "package example:notes@1.0.0;\n\ninterface notes {}\n",
    )
    .unwrap();
    std::fs::write(&file, TALLY).unwrap();
    assert_eq!(
        write_dependencies(&deps, &dependencies).unwrap(),
        [deps.join("tembra-guest.wit")]
    );
    assert!(!former.exists() && notes.exists());
    // The directory resolves on its own, as another Orb's bindings read it, and reads as the
    // contract itself.
    Resolve::default().push_dir(&dir).unwrap();
    let read = Contract::from_dir(&dir, Some("first")).unwrap();
    assert_eq!(read.prepared(), contract.prepared());
    // A contract that depends on no standard package leaves no deps/ behind.
    std::fs::remove_file(&notes).unwrap();
    assert!(write_dependencies(&deps, &[]).unwrap().is_empty());
    assert!(!deps.exists());
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
