//! WASI imports: `import = "wasi:..."` names an interface of the WASI packages bundled with
//! `tembra-contracts`, which the inline WIT nests unless the consumed directory holds them.
use super::{Options, expand};
use std::path::{Path, PathBuf};

const DICE: &str = "mod dice {
    #[service]
    pub struct Chance;
    impl Chance {
        pub fn draw(length: u64) -> Vec<u8> { random::get_random_bytes(length) }
    }
}";

fn literal(text: &str) -> syn::LitStr {
    syn::LitStr::new(text, proc_macro2::Span::call_site())
}

fn importing(imports: &[&str], wit: Option<&Path>) -> Options {
    Options {
        package: Some(literal("example:dice@1.0.0")),
        wit: wit.map(|dir| dir.display().to_string()),
        imports: imports.iter().map(|import| literal(import)).collect(),
        ..Options::default()
    }
}

/// The generated module as one token string, or the macro's error.
fn expanded(options: Options, module: &str) -> syn::Result<String> {
    expand(options, syn::parse_str(module).expect("a module")).map(|tokens| tokens.to_string())
}

/// The inline WIT the bindings read, from the generated tokens.
fn inline(generated: &str) -> String {
    let start = generated.find("inline : ").expect("an inline WIT") + "inline : ".len();
    let literal: syn::LitStr =
        syn::parse_str(generated[start..].split(" , world").next().unwrap()).unwrap();
    literal.value()
}

/// A directory holding the contract of an Orb that imports a WASI interface, as `cargo tembra
/// build` writes it: the world, and in `deps/` each given WASI package as its file.
fn contract(name: &str, deps: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tembra-orb-macro-wasi-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("deps")).unwrap();
    std::fs::write(
        dir.join("timer.wit"),
        "package example:timer@1.0.0;\n\ninterface timer {\n  now: func() -> u64;\n}\n\nworld timer-world {\n  import wasi:clocks/monotonic-clock@0.3.0;\n  export timer;\n}\n",
    )
    .unwrap();
    for (file, source) in deps {
        std::fs::write(dir.join("deps").join(file), source).unwrap();
    }
    dir
}

/// The bundled source of a WASI package, by its file name.
fn bundled(file: &str) -> &'static str {
    tembra_contracts::WASI_SOURCES
        .iter()
        .find(|(name, _)| *name == file)
        .map(|(_, source)| *source)
        .expect("a bundled WASI package")
}

/// A WASI interface is imported without a consumed directory: the world imports it, its module is
/// re-exported under its name, and its package is nested in the inline WIT from the bundled source.
#[test]
fn a_wasi_import_needs_no_consumed_directory() {
    let generated = expanded(importing(&["wasi:random/random@0.3.0"], None), DICE)
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(
        generated.contains("import wasi:random/random@0.3.0;\\n  export chance;"),
        "{generated}"
    );
    assert!(
        generated.contains("pub use self :: wasi :: random :: random ;"),
        "{generated}"
    );
    assert!(!generated.contains("path :"), "{generated}");
    // Nothing for the manifest: no hook, no policy, no consumed directory.
    assert!(!generated.contains("tembra:manifest"), "{generated}");
    let inline = inline(&generated);
    assert!(inline.contains("package wasi:random@0.3.0 {"), "{inline}");
    assert!(!inline.contains("package wasi:clocks@0.3.0 {"), "{inline}");
}

/// A WASI package comes with the bundled packages it builds on, and the inline WIT resolves on its
/// own, as the bindings read it: a world importing `preopens` imports the filesystem's types and the
/// system clock their timestamps use. A WASI interface is not read as a consumed one, whose
/// functions the macro would spell: `stdout` writes a stream, which no Orb function may take.
#[test]
fn a_wasi_package_comes_with_the_packages_it_builds_on() {
    let generated = expanded(
        importing(
            &["wasi:filesystem/preopens@0.3.0", "wasi:cli/stdout@0.3.0"],
            None,
        ),
        DICE,
    )
    .unwrap_or_else(|error| panic!("{error}"));
    let inline = inline(&generated);
    let nested: Vec<&str> = inline
        .lines()
        .filter_map(|line| line.strip_prefix("package wasi:")?.strip_suffix(" {"))
        .collect();
    // The packages the filesystem and the CLI build on come with them, in the order the bundled
    // sources list them; HTTP does not.
    assert_eq!(
        nested,
        [
            "clocks@0.3.0",
            "random@0.3.0",
            "filesystem@0.3.0",
            "sockets@0.3.0",
            "cli@0.3.0"
        ]
    );
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve
        .push_str("dice.wit", &inline)
        .unwrap_or_else(|error| panic!("{error:#}"));
    let world = resolve.select_world(&[package], Some("dice")).unwrap();
    let imported: Vec<String> = resolve.worlds[world]
        .imports
        .keys()
        .map(|key| resolve.name_world_key(key))
        .collect();
    for interface in [
        "wasi:clocks/system-clock@0.3.0",
        "wasi:filesystem/types@0.3.0",
        "wasi:filesystem/preopens@0.3.0",
        "wasi:cli/stdout@0.3.0",
    ] {
        assert!(
            imported.iter().any(|name| name == interface),
            "{imported:?}"
        );
    }
}

/// The `wasi` namespace holds the bundled packages and nothing else: an identity naming no
/// interface of them, another version, an unknown interface or an unstable one, is an error naming
/// what there is instead.
#[test]
fn a_wasi_identity_names_an_interface_of_the_bundled_packages() {
    for (identity, expected) in [
        (
            "wasi:random/random@0.2.0",
            "wasi:random/random@0.2.0 is not an interface of the WASI packages Tembra serves: wasi:clocks@0.3.0, wasi:random@0.3.0, wasi:filesystem@0.3.0, wasi:sockets@0.3.0, wasi:cli@0.3.0, wasi:http@0.3.0",
        ),
        (
            "wasi:keyvalue/store@0.3.0",
            "wasi:keyvalue/store@0.3.0 is not an interface of the WASI packages Tembra serves",
        ),
        (
            "wasi:random/secure@0.3.0",
            "wasi:random/secure@0.3.0 is not an interface of wasi:random@0.3.0, which has insecure-seed, insecure, random",
        ),
        (
            "wasi:clocks/timezone@0.3.0",
            "wasi:clocks/timezone@0.3.0 is not an interface of wasi:clocks@0.3.0, which has types, monotonic-clock, system-clock",
        ),
    ] {
        let error = expanded(importing(&[identity], None), DICE)
            .map(|_| ())
            .unwrap_err()
            .to_string();
        assert!(error.contains(expected), "{identity}: {error}");
    }
    // Another Orb's interface still comes from the consumed directory.
    let error = expanded(
        importing(
            &["wasi:random/random@0.3.0", "example:timer/timer@1.0.0"],
            None,
        ),
        DICE,
    )
    .map(|_| ())
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("implementing or importing an interface needs wit = \"<dir>\""),
        "{error}"
    );
}

/// A contract `cargo tembra build` wrote for an Orb that imports WASI holds the WASI packages in
/// `deps/`: an Orb consuming it reads each such package once, from there, the bundled copy left out
/// of its inline WIT whatever the copy's comments say, and nests only the WASI packages the
/// directory lacks; a copy defining a package otherwise is an error naming its file.
#[test]
fn a_consumed_contract_may_bring_the_wasi_packages() {
    let clocks = bundled("wasi-clocks.wit");
    let imports = [
        "example:timer/timer@1.0.0",
        "wasi:clocks/monotonic-clock@0.3.0",
        "wasi:random/random@0.3.0",
    ];
    let commented = clocks
        .replace("/// ", "/// (another text) ")
        .replace('\n', "\r\n");
    for (name, copy) in [("deps", clocks), ("comments", commented.as_str())] {
        let dir = contract(name, &[("wasi-clocks.wit", copy)]);
        let generated = expanded(importing(&imports, Some(&dir)), DICE)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        let inline = inline(&generated);
        assert!(
            !inline.contains("package wasi:clocks@0.3.0"),
            "{name}: {inline}"
        );
        assert!(
            inline.contains("package wasi:random@0.3.0 {"),
            "{name}: {inline}"
        );
        assert!(
            generated.contains("pub use self :: wasi :: clocks :: monotonic_clock ;")
                && generated.contains("pub use self :: example :: timer :: timer ;"),
            "{generated}"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
    let different = clocks.replacen(
        "get-resolution: func() -> duration;",
        "get-resolution: func() -> u32;",
        1,
    );
    let dir = contract("different", &[("wasi-clocks.wit", &different)]);
    let error = expanded(importing(&imports, Some(&dir)), DICE)
        .map(|_| ())
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("deps/wasi-clocks.wit:")
            && error.contains("this copy of wasi:clocks@0.3.0, a WASI package Tembra serves, defines it otherwise than the one bundled with this macro"),
        "{error}"
    );
    std::fs::remove_dir_all(dir).unwrap();
}
