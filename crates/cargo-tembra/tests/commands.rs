//! The `cargo tembra` commands, run as a user runs them. The crate `new` writes builds as a
//! component and natively. `build --out` collects its outputs in one directory. `wit` and
//! `inspect` read the component back, and `inspect` names the grants a WASI import needs.
//!
//! Each test scaffolds its crates in a temporary directory. The `target/` of each crate is a link
//! to one directory in the target directory of the tests, so the dependencies compile once, not
//! once per crate. A build from a cold target directory is slow, so the tests are ignored.
//! `just test-cargo-tembra` runs them.
use sha2::{Digest, Sha256};
use std::{
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
        .output()
        .unwrap();
    printed(&format!("cargo tembra {}", arguments.join(" ")), output)
}

/// Run Cargo with `arguments` in `directory`.
fn cargo(directory: &Path, arguments: &[&str]) {
    let output = Command::new(env!("CARGO"))
        .args(arguments)
        .current_dir(directory)
        .output()
        .unwrap();
    printed(&format!("cargo {}", arguments.join(" ")), output);
}

/// Make the crate build into the directory every crate of these tests shares. `cargo tembra build`
/// always builds into `target/` beside the manifest, so that is where the link goes.
#[cfg(unix)]
fn share_target(krate: &Path) {
    let shared = Path::new(env!("CARGO_TARGET_TMPDIR")).join("cargo-tembra-tests");
    std::fs::create_dir_all(&shared).unwrap();
    std::os::unix::fs::symlink(shared, krate.join("target")).unwrap();
}

/// Without links, each crate builds into its own `target/`.
#[cfg(not(unix))]
fn share_target(_krate: &Path) {}

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
    share_target(&krate);
    (krate, printed)
}

/// The crate `new` writes builds as a component, whose outputs `build --out` collects in the
/// directory it is given, and natively, with and without the facade's host runtime. A native
/// build writes the contract that the component build wrote.
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
    cargo(&krate, &["build", "--features", "tembra/host"]);
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
    let (krate, _) = scaffold(&dir, NAME, PACKAGE);
    let manifest = krate.join("Cargo.toml");
    tembra(&dir, &["build", "--manifest-path", text(&manifest)]);
    let component = krate.join("artifacts/greeter-orb.wasm");
    let contract = read(&krate.join("contract/greeter-orb.wit"));

    let printed = tembra(&dir, &["wit", text(&component)]);
    for expected in [
        "export example:greeter-orb/api@1.0.0;",
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
            NAME,
        ],
    );
    let file = emitted.join("greeter-orb.wit");
    assert_eq!(read(&file), contract);
    assert_eq!(printed, format!("written {}\n{contract}", file.display()));

    let printed = tembra(&dir, &["inspect", text(&component)]);
    for expected in [
        "orb::example:greeter-orb/api@1.0.0  methods: hello\n",
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
            NAME,
        ],
    );
    let verdict = format!(
        "checked against world greeter-orb of {}:\n  ACCEPTED, artifact of ",
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
