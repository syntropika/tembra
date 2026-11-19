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
/// workspace enables it, since Cargo unifies the features of the members it builds. The guests'
/// modules compile all the same, so `cargo check --workspace` passes: both hook declarations and
/// contributions explicitly refuse native installation without the runtime's `hooks` feature,
/// which the application leaves out.
#[test]
#[ignore = "runs cargo on a fixture: just test-configurations"]
fn a_workspace_whose_application_enables_host_checks_beside_a_guest() {
    let manifest = fixture("workspace/Cargo.toml");
    if let Err(errors) = cargo(&manifest, &["check", "--workspace"]) {
        panic!("{errors}");
    }
}

/// A `no_std` guest builds for wasm32 whatever the facade's features: without one that links std,
/// on its module's panic handler, and with `host` or `contracts`, which another crate of its build
/// may enable, on std's, which the facade then links in its place.
#[test]
#[ignore = "runs cargo on a fixture: just test-configurations"]
fn a_freestanding_guest_builds_for_wasm32_whatever_links_std() {
    let manifest = fixture("freestanding/Cargo.toml");
    let failed = ["", "host", "contracts"]
        .into_iter()
        .filter_map(|features| {
            let args = [
                "build",
                "--lib",
                "--target",
                "wasm32-unknown-unknown",
                "--features",
                features,
            ];
            cargo(&manifest, &args)
                .err()
                .map(|errors| format!("--features '{features}'\n{errors}"))
        })
        .collect::<Vec<_>>();
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

/// A `no_std` guest whose `crate = ".."` names no Rust identifier reports the macro's refusal
/// alone, natively and for wasm32: its runtime prelude names no facade the build cannot resolve,
/// and still brings the panic handler a freestanding crate needs.
#[test]
#[ignore = "runs cargo on a fixture: just test-configurations"]
fn a_refused_facade_name_reports_the_refusal_alone() {
    let manifest = fixture("refused-facade/Cargo.toml");
    for target in [&[][..], &["--target", "wasm32-unknown-unknown"]] {
        let errors = cargo(&manifest, &[&["build", "--lib"][..], target].concat())
            .expect_err("the macro refuses the attribute");
        assert!(
            errors.contains("`crate` names the facade dependency as the crate's code writes it, a Rust identifier"),
            "{target:?}\n{errors}"
        );
        assert!(
            errors.contains("due to 1 previous error"),
            "{target:?}\n{errors}"
        );
    }
}

/// A manifest in TOML 1.1 syntax, with a trailing comma in an inline table, which Cargo reads:
/// the macro reads it too, for the Orb's name in `orb()`, so the crate builds natively.
#[test]
#[ignore = "runs cargo on a fixture: just test-configurations"]
fn a_manifest_in_toml_1_1_builds_natively() {
    let manifest = fixture("toml-1-1/Cargo.toml");
    if let Err(errors) = cargo(&manifest, &["build", "--features", "host"]) {
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

/// A crate with two Orb modules builds natively, each module an Orb of its own, but a component
/// holds one Orb: built as one, the crate fails on its second module, with an error that says why
/// and how to split the crate, and not first on the kernel's interfaces both modules export.
#[test]
#[ignore = "runs cargo on a fixture: just test-configurations"]
fn a_crate_with_two_orbs_builds_natively_and_fails_as_a_component() {
    let manifest = fixture("two-orbs/Cargo.toml");
    if let Err(errors) = cargo(&manifest, &["build", "--lib"]) {
        panic!("{errors}");
    }
    let errors = cargo(&manifest, &COMPONENT).expect_err("a component of two Orbs builds");
    assert!(
        errors.contains(&format!("symbol `{ONE_ORB}` is already defined")),
        "{errors}"
    );
    assert!(
        errors.contains("src/lib.rs:29:"),
        "not on the second module\n{errors}"
    );
}

/// An Orb crate whose dependency holds an Orb module too builds natively, but a component holds
/// one Orb, its dependencies' included: built as one, the crate fails at the link, with the same
/// words as a crate holding two.
#[test]
#[ignore = "runs cargo on a fixture: just test-configurations"]
fn an_orb_crate_depending_on_another_builds_natively_and_fails_as_a_component() {
    let manifest = fixture("orb-dependency/Cargo.toml");
    let consumer = ["-p", "tembra-config-consumer"];
    if let Err(errors) = cargo(&manifest, &[&["build", "--lib"][..], &consumer].concat()) {
        panic!("{errors}");
    }
    let errors = cargo(&manifest, &[&COMPONENT[..], &consumer].concat())
        .expect_err("a component of an Orb crate and its Orb dependency builds");
    assert!(
        errors.contains(&format!("duplicate symbol: {ONE_ORB}")),
        "{errors}"
    );
}

/// The arguments that build a crate's library as a component.
const COMPONENT: [&str; 8] = [
    "rustc",
    "--lib",
    "--target",
    "wasm32-unknown-unknown",
    "--crate-type",
    "cdylib",
    "--config",
    "target.wasm32-unknown-unknown.linker = \"wasm-component-ld\"",
];

/// The symbol every Orb module defines for wasm32, whose name is the error of a build that holds
/// two Orb modules.
const ONE_ORB: &str = "A component holds one Orb module, of this crate or of a dependency, and \
    this build holds more than one: put each Orb module in a crate of its own, and what another \
    crate uses of it in a crate without one";

/// The facade's manifest.
const FACADE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../tembra/Cargo.toml");

/// Check the facade without its default features, with `features` and `args`, and return Cargo's
/// output, under the command line, when it fails.
fn check_facade(features: &str, args: &[&str]) -> Result<(), String> {
    let check = ["check", "--no-default-features", "--features", features];
    cargo(Path::new(FACADE), &[&check[..], args].concat())
        .map_err(|errors| format!("--features '{features}' {}\n{errors}", args.join(" ")))
}

/// The facade compiles natively with no feature and with each feature of its manifest alone, so
/// each feature enables whatever its items need. Only `default` is left out: it is `host`.
#[test]
#[ignore = "runs cargo on the facade: just test-configurations"]
fn the_facade_checks_with_each_feature_alone() {
    let manifest: toml::Table = std::fs::read_to_string(FACADE).unwrap().parse().unwrap();
    let features = manifest["features"]
        .as_table()
        .expect("the facade's features")
        .keys()
        .map(String::as_str)
        .filter(|feature| *feature != "default");
    let failed = std::iter::once("")
        .chain(features)
        .filter_map(|feature| check_facade(feature, &[]).err())
        .collect::<Vec<_>>();
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

/// The facade compiles for wasm32 with `macros`, as a guest's build compiles it, and with `host`
/// too, which a guest gets when another crate of the build enables it. `__host!` must drop what it
/// wraps there, or the facade's assertion inside it, that the build is not for wasm32, fails.
#[test]
#[ignore = "runs cargo on the facade: just test-configurations"]
fn the_facade_checks_for_wasm32_with_macros() {
    let failed = ["macros", "macros,host"]
        .into_iter()
        .filter_map(|features| {
            check_facade(features, &["--target", "wasm32-unknown-unknown"]).err()
        })
        .collect::<Vec<_>>();
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

/// Canonical capability packages compose in place and Cargo supplies the only output directory.
#[test]
#[ignore = "runs cargo on a fixture: just test-configurations"]
fn canonical_packages_emit_the_contract_in_cargos_output_directory() {
    let manifest = fixture("canonical-packages/Cargo.toml");
    if let Err(errors) = cargo(&manifest, &["build", "--lib"]) {
        panic!("{errors}");
    }
    let build = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .parent()
        .unwrap()
        .join("config-fixtures/debug/build");
    let output = std::fs::read_dir(build)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("out/composed.wit"))
        .find(|path| path.is_file())
        .expect("the primary library emits into OUT_DIR");
    let text = std::fs::read_to_string(&output).unwrap();
    assert!(text.contains("record session"), "{text}");
    assert!(text.contains("example:model-agent/agent@1.0.0"), "{text}");
    tembra_contracts::Contract::from_dir(output.parent().unwrap(), Some("composed")).unwrap();
    assert!(!manifest.parent().unwrap().join("contract").exists());
    if let Err(errors) = cargo(&manifest, &COMPONENT) {
        panic!("{errors}");
    }
}
