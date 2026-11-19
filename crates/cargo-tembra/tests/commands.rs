//! The `cargo tembra` commands, run as a user runs them. The crate `new` writes builds as a
//! component and natively. `build --out` collects its outputs in one directory. `wit` and
//! `inspect` read the component back, and `inspect` names the grants a WASI import needs.
//!
//! Each test scaffolds its crates in a temporary directory. The crates build into one target
//! directory, which `CARGO_TARGET_DIR` names, so the dependencies compile once, not once per
//! crate. Each crate starts from a lockfile of the repository, so a run resolves nothing
//! from the registry, and a release published or yanked since cannot change its result. A build
//! from a cold target directory is slow, so the tests are ignored. `just test-cargo-tembra` runs
//! them.
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::{Command, Output},
};

/// A hyphenated name, which each tool spells its own way: `greeter-orb` for the crate, the Orb and
/// the world, `greeter_orb` for the library and its module.
const NAME: &str = "greeter-orb";

/// The WIT package of the crate named `NAME`.
const PACKAGE: &str = "example:greeter-orb@1.0.0";

/// A module that imports two WASI interfaces: the host's secure random source and its monotonic
/// clock. Waiting on the clock is async, which the bindings serve with the `async` feature of
/// `wit-bindgen`.
const DICE: &str = r#"#![cfg_attr(target_arch = "wasm32", no_std)]
extern crate alloc;

#[tembra::orb(
    package = "example:dice@1.0.0",
    import = "wasi:random/random@0.3.0",
    import = "wasi:clocks/monotonic-clock@0.3.0",
    no_std
)]
mod dice {
    pub mod die {
        use super::{monotonic_clock, random};

        #[service]
        pub struct Die;

        impl Die {
            /// A throw of the die, from the host's secure random source.
            pub fn throw() -> u8 {
                (random::get_random_u64() % 6) as u8 + 1
            }

            /// Wait on the host's monotonic clock, then throw.
            pub async fn throw_after(nanoseconds: u64) -> u8 {
                monotonic_clock::wait_for(nanoseconds).await;
                Die::throw()
            }
        }
    }
}
"#;

/// An empty directory for one test, under the system's temporary directory.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cargo-tembra-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A path as the text of a command line; the paths of these tests are UTF-8.
fn text(path: &Path) -> &str {
    path.to_str().unwrap()
}

/// The text of a file. A file that is missing fails the test with its path.
fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// What a finished command wrote to its standard output. A command that failed fails the test
/// with both of its streams.
fn printed(command: &str, output: Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "{command}: {}\n--- stdout\n{stdout}\n--- stderr\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    stdout
}

/// Run `cargo tembra` with `arguments` in `directory`. Cargo starts a subcommand with its own
/// name first, so the binary gets `tembra` before the arguments.
fn tembra(directory: &Path, arguments: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_cargo-tembra"))
        .arg("tembra")
        .args(arguments)
        .current_dir(directory)
        .env("CARGO_TARGET_DIR", shared_target())
        .output()
        .unwrap();
    printed(&format!("cargo tembra {}", arguments.join(" ")), output)
}

/// Run Cargo with `arguments` in `directory`.
fn cargo(directory: &Path, arguments: &[&str]) {
    let output = Command::new(env!("CARGO"))
        .args(arguments)
        .current_dir(directory)
        .env("CARGO_TARGET_DIR", shared_target())
        .output()
        .unwrap();
    printed(&format!("cargo {}", arguments.join(" ")), output);
}

/// The target directory every crate of these tests shares, which the commands that build take
/// from `CARGO_TARGET_DIR`, set for the processes the tests spawn only. The tests run in parallel
/// and Cargo names its files after the crate, so each test that builds a crate gives it a name of
/// its own: no other test's build overwrites a file the test reads.
fn shared_target() -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join("cargo-tembra-tests")
}

/// The lockfile each crate starts from. Cargo keeps the versions a lockfile holds and resolves
/// from the registry only what it lacks. The runtime fixture of the macro depends on what `new`
/// writes into a manifest, the facade with `macros`, `dlmalloc`, `serde` and `serde_json`, and on
/// the facade's `host` runtime for its native tests. The recipes that build it use `--locked`, so
/// its lockfile holds all of them and stays current. The lockfile of the workspace does not
/// serve: it has no `dlmalloc`.
fn lockfile() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../orb-macro/runtime-fixture/Cargo.lock")
}

/// Start the crate from the lockfile of the repository.
fn prepare(krate: &Path) {
    std::fs::copy(lockfile(), krate.join("Cargo.lock")).unwrap();
}

/// The packages a lockfile takes from a registry, by name and version, with their checksums.
fn registry_packages(path: &Path) -> BTreeMap<String, String> {
    let lock: toml::Value = toml::from_str(&read(path)).unwrap();
    let field = |package: &toml::Value, key: &str| package[key].as_str().unwrap().to_owned();
    lock["package"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|package| package.get("source").is_some())
        .map(|package| {
            let name = format!("{} {}", field(package, "name"), field(package, "version"));
            (name, field(package, "checksum"))
        })
        .collect()
}

/// Check that the builds of the crate resolved nothing from the registry. Cargo writes the
/// packages the crate needs back to its lockfile. A package that Cargo had to resolve is missing
/// from the lockfile the crate started from, or has another version or checksum there.
fn assert_pinned(krate: &Path) {
    let pinned = registry_packages(&lockfile());
    let resolved: Vec<String> = registry_packages(&krate.join("Cargo.lock"))
        .into_iter()
        .filter(|(package, checksum)| pinned.get(package) != Some(checksum))
        .map(|(package, _)| package)
        .collect();
    assert!(
        resolved.is_empty(),
        "{}: resolved from the registry, not held by {}: {resolved:?}",
        krate.display(),
        lockfile().display()
    );
}

/// Run `cargo tembra new` in `directory` for the crate `name` of the WIT package `package`, with
/// the facade of this checkout. Returns the new crate and what the command printed.
fn scaffold(directory: &Path, name: &str, package: &str) -> (PathBuf, String) {
    let checkout = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let printed = tembra(
        directory,
        &[
            "new",
            name,
            "--package",
            package,
            "--tembra",
            text(&checkout),
        ],
    );
    let krate = directory.join(name);
    prepare(&krate);
    (krate, printed)
}

/// The crate `new` writes builds as a component, whose outputs `build --out` collects in the
/// directory it is given, and natively, without the facade's host runtime and with it, which a
/// feature of the manifest enables. A native build writes the contract that the component build
/// wrote.
#[test]
#[ignore = "builds scratch crates, which is slow from a cold target directory; run by `just test-cargo-tembra`"]
fn a_new_crate_builds_as_a_component_and_natively() {
    let dir = scratch("new");
    let (krate, printed) = scaffold(&dir, NAME, PACKAGE);
    assert!(
        printed.starts_with("created greeter-orb/ with tembra from "),
        "{printed}"
    );
    assert!(
        printed.ends_with("  cd greeter-orb && cargo tembra build\n"),
        "{printed}"
    );
    for file in ["Cargo.toml", "src/lib.rs", ".gitignore"] {
        assert!(krate.join(file).is_file(), "{file}");
    }
    // The manifest names the facade's `host` feature, as the manifest of an author who builds
    // natively does. A lockfile holds the packages of the features that manifests name, so
    // `--features tembra/host` on the command line would resolve those of the runtime from the
    // registry at each build.
    let manifest = krate.join("Cargo.toml");
    let features = "\n[features]\nhost = [\"tembra/host\"]\n";
    std::fs::write(&manifest, read(&manifest) + features).unwrap();

    // As a component, built where `new` says to build it, with the outputs collected in `out`
    // instead of `artifacts/`.
    let out = dir.join("out");
    let printed = tembra(&krate, &["build", "--out", text(&out)]);
    let summary = "exports, the services of greeter-orb:\n  \
                   greeter-orb::example:greeter-orb/api@1.0.0  methods: hello\n\
                   imports: none; the installer grants nothing\n";
    assert!(printed.ends_with(summary), "{printed}");
    let mut files: Vec<String> = std::fs::read_dir(&out)
        .expect("--out names the directory the outputs go to")
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    files.sort();
    assert_eq!(
        files,
        [
            "checksums.txt",
            "greeter-orb.orb.wasm",
            "greeter-orb.wasm",
            "greeter-orb.wit",
        ]
    );
    assert!(
        !krate.join("artifacts").exists(),
        "--out replaces artifacts/"
    );
    let component = std::fs::read(out.join("greeter-orb.wasm")).unwrap();
    assert!(wasmparser::Parser::is_component(&component));
    let artifact = std::fs::read(out.join("greeter-orb.orb.wasm")).unwrap();
    let identity = tembra_contracts::Artifact::decode(&artifact)
        .unwrap()
        .identity;
    assert_eq!(
        (
            identity.name.as_str(),
            identity.api_version,
            identity.world.as_str()
        ),
        ("greeter-orb", 1, "greeter-orb")
    );
    // The checksums are those of the files, sorted by name.
    let checksums: String = [
        "greeter-orb.orb.wasm",
        "greeter-orb.wasm",
        "greeter-orb.wit",
    ]
    .iter()
    .map(|file| {
        let digest = Sha256::digest(std::fs::read(out.join(file)).unwrap());
        format!("{digest:x}  {file}\n")
    })
    .collect();
    assert_eq!(read(&out.join("checksums.txt")), checksums);
    let written = read(&krate.join("contract/greeter-orb.wit"));
    assert!(written.contains("world greeter-orb {"), "{written}");
    assert_eq!(
        read(&out.join("greeter-orb.wit")),
        written,
        "the copy of the contract in `out`"
    );

    // Natively, first as an author's `cargo build` compiles it, without the host runtime, then with
    // the runtime that the facade's `host` feature links, which gives the module its `orb()`. The
    // macro writes the contract in a native build too: the same text, which the first build writes
    // again after the contract is removed.
    std::fs::remove_dir_all(krate.join("contract")).unwrap();
    cargo(&krate, &["build"]);
    assert_eq!(
        read(&krate.join("contract/greeter-orb.wit")),
        written,
        "the contract of the native build"
    );
    cargo(&krate, &["build", "--features", "host"]);
    // A native host is another crate: it reaches the module `new` wrote by its path, and builds
    // the Orb the module's `orb()` names. An integration test of the crate is such a host. The
    // service sits in a module of its own, `api`, which the host reaches below the Orb's module.
    std::fs::create_dir_all(krate.join("tests")).unwrap();
    std::fs::write(
        krate.join("tests/host.rs"),
        "#[test]\nfn the_module_builds_its_orb() {\n    greeter_orb::greeter_orb::orb().unwrap();\n}\n\n\
         #[test]\nfn the_service_sits_in_its_module() {\n    \
         assert_eq!(greeter_orb::greeter_orb::api::Api::hello(\"Ada\".into()), \"Hello, Ada!\");\n}\n",
    )
    .unwrap();
    cargo(&krate, &["test", "--features", "host", "--test", "host"]);
    assert_pinned(&krate);
    let locked = registry_packages(&krate.join("Cargo.lock"));
    assert!(
        locked.keys().any(|package| package.starts_with("futures ")),
        "the lockfile holds the packages of the runtime"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

/// A package with a binary beside its library builds: `build` compiles the library target, so
/// Cargo does not refuse the extra arguments for a package of two targets.
#[test]
#[ignore = "builds scratch crates, which is slow from a cold target directory; run by `just test-cargo-tembra`"]
fn a_package_with_a_binary_builds_its_library() {
    let dir = scratch("with-bin");
    let (krate, _) = scaffold(&dir, NAME, PACKAGE);
    std::fs::write(krate.join("src/main.rs"), "fn main() {}\n").unwrap();
    let printed = tembra(&krate, &["build"]);
    assert!(
        printed.contains("greeter-orb::example:greeter-orb/api@1.0.0"),
        "{printed}"
    );
    let component = std::fs::read(krate.join("artifacts/greeter-orb.wasm")).unwrap();
    assert!(wasmparser::Parser::is_component(&component));
    std::fs::remove_dir_all(dir).unwrap();
}

/// A manifest that uses TOML 1.1 syntax, a trailing comma in an inline table, which Cargo reads,
/// is read by `build`, and by the macro in a native build.
#[test]
#[ignore = "builds scratch crates, which is slow from a cold target directory; run by `just test-cargo-tembra`"]
fn a_manifest_with_a_trailing_comma_builds_both_ways() {
    let dir = scratch("trailing-comma");
    let (krate, _) = scaffold(&dir, NAME, PACKAGE);
    let manifest = krate.join("Cargo.toml");
    let features = "\n[package.metadata.other]\nkey = { a = 1, b = 2, }\n\n\
                    [features]\nhost = [\"tembra/host\"]\n";
    std::fs::write(&manifest, read(&manifest) + features).unwrap();
    tembra(&krate, &["build"]);
    assert!(krate.join("artifacts/greeter-orb.wasm").is_file());
    cargo(&krate, &["build", "--features", "host"]);
    std::fs::remove_dir_all(dir).unwrap();
}

/// `wit` prints the WIT of the component that `build` wrote and, given `--emit` and the world,
/// writes it to a file. The macro embedded its own text in the component, so the file holds that
/// text, comments included, as the contract `build` wrote. `inspect` lists what the component
/// exports and imports, and checks it against that contract.
#[test]
#[ignore = "builds scratch crates, which is slow from a cold target directory; run by `just test-cargo-tembra`"]
fn wit_and_inspect_read_back_the_component_a_build_wrote() {
    let dir = scratch("read-back");
    let (krate, _) = scaffold(&dir, "reader-orb", "example:reader-orb@1.0.0");
    let manifest = krate.join("Cargo.toml");
    tembra(&dir, &["build", "--manifest-path", text(&manifest)]);
    assert_pinned(&krate);
    let component = krate.join("artifacts/reader-orb.wasm");
    let contract = read(&krate.join("contract/reader-orb.wit"));

    let printed = tembra(&dir, &["wit", text(&component)]);
    for expected in [
        "export example:reader-orb/api@1.0.0;",
        "hello: func(name: string) -> string;",
    ] {
        assert!(printed.contains(expected), "{expected}\n{printed}");
    }
    // The directory to emit into does not exist yet.
    let emitted = dir.join("emitted");
    let printed = tembra(
        &dir,
        &[
            "wit",
            text(&component),
            "--emit",
            text(&emitted),
            "--world",
            "reader-orb",
        ],
    );
    let file = emitted.join("reader-orb.wit");
    assert_eq!(read(&file), contract);
    assert_eq!(printed, format!("written {}\n{contract}", file.display()));

    let printed = tembra(&dir, &["inspect", text(&component)]);
    for expected in [
        "orb::example:reader-orb/api@1.0.0  methods: hello\n",
        "imports: none; the installer grants nothing\n",
    ] {
        assert!(printed.contains(expected), "{expected}\n{printed}");
    }
    let wit = krate.join("contract");
    let printed = tembra(
        &dir,
        &[
            "inspect",
            text(&component),
            "--wit",
            text(&wit),
            "--world",
            "reader-orb",
        ],
    );
    let verdict = format!(
        "checked against world reader-orb of {}:\n  ACCEPTED, artifact of ",
        wit.display()
    );
    assert!(printed.contains(&verdict), "{verdict}\n{printed}");
    std::fs::remove_dir_all(dir).unwrap();
}

/// `inspect` names the grants an installer must give for the WASI interfaces a component imports,
/// and lists the interfaces its core module imports. A component that imports `random` and
/// `monotonic-clock` needs the grants of `random` and `clocks`. The contract `build` wrote holds
/// the WASI packages in `deps/`, and `inspect` accepts the component against it.
#[test]
#[ignore = "builds scratch crates, which is slow from a cold target directory; run by `just test-cargo-tembra`"]
fn inspect_names_the_grants_a_wasi_import_needs() {
    let dir = scratch("grants");
    let (krate, _) = scaffold(&dir, "dice", "example:dice@1.0.0");
    // An author writes the module over the one `new` wrote.
    std::fs::write(krate.join("src/lib.rs"), DICE).unwrap();
    let manifest = krate.join("Cargo.toml");
    tembra(&dir, &["build", "--manifest-path", text(&manifest)]);
    assert_pinned(&krate);
    let component = krate.join("artifacts/dice.wasm");

    let printed = tembra(&dir, &["inspect", text(&component)]);
    for expected in [
        "  wasi:clocks/monotonic-clock@0.3.0: ",
        "  wasi:random/random@0.3.0: get-random-u64\n",
        "orb::example:dice/die@1.0.0  methods: throw, throw-after\n",
        "WASI imports, grants the installer must give:\n  \
         system: clocks: wasi:clocks/monotonic-clock@0.3.0\n  \
         system: random: wasi:random/random@0.3.0\n",
    ] {
        assert!(printed.contains(expected), "{expected}\n{printed}");
    }
    let wit = krate.join("contract");
    for dependency in ["wasi-clocks.wit", "wasi-random.wit"] {
        assert!(wit.join("deps").join(dependency).is_file(), "{dependency}");
    }
    let printed = tembra(
        &dir,
        &[
            "inspect",
            text(&component),
            "--wit",
            text(&wit),
            "--world",
            "dice",
        ],
    );
    assert!(printed.contains("  ACCEPTED, artifact of "), "{printed}");
    std::fs::remove_dir_all(dir).unwrap();
}

/// `build` writes where Cargo would: the `target_directory` of `cargo metadata`, which follows
/// `CARGO_TARGET_DIR`. The README names the path of the component there, and a crate whose
/// `target/` does not exist gets none.
#[test]
#[ignore = "builds scratch crates, which is slow from a cold target directory; run by `just test-cargo-tembra`"]
fn build_writes_the_component_into_the_target_directory_of_cargo() {
    let dir = scratch("target-dir");
    let (krate, _) = scaffold(&dir, "probe-orb", "example:probe-orb@1.0.0");
    let target = shared_target();
    tembra(&krate, &["build"]);
    let built = target.join("wasm32-unknown-unknown/release/probe_orb.wasm");
    assert!(built.is_file(), "{}", built.display());
    assert!(
        !krate.join("target").exists(),
        "target/ beside the manifest"
    );
    // The copy in artifacts/ is the file Cargo wrote.
    assert_eq!(
        std::fs::read(krate.join("artifacts/probe-orb.wasm")).unwrap(),
        std::fs::read(&built).unwrap()
    );
    std::fs::remove_dir_all(dir).unwrap();
}
