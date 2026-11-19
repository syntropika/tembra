//! WASI imports: `import = "wasi:..."` names an interface of the WASI packages bundled with
//! `tembra-contracts`, which the inline WIT nests unless the consumed directory holds them.
use super::tests::literal;
use super::{Options, expand, metadata::Metadata};
use std::path::{Path, PathBuf};

const DICE: &str = "mod dice {
    pub mod chance {
        #[service]
        pub struct Chance;
        impl Chance {
            pub fn draw(length: u64) -> Vec<u8> { super::random::get_random_bytes(length) }
        }
    }
}";

/// `DICE` with a state, which the kernel keeps: the bindings read the kernel's package.
const DICE_WITH_STATE: &str = "mod dice {
    pub mod chance {
        #[state]
        pub struct Rolls { pub count: u32 }
        #[service]
        pub struct Chance;
        impl Chance {
            pub fn draw(length: u64) -> Vec<u8> { super::random::get_random_bytes(length) }
        }
    }
}";

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
    expand(
        options,
        syn::parse_str(module).expect("a module"),
        Ok(Metadata::named("dice")),
        None,
    )
    .map(|tokens| tokens.to_string())
}

/// The inline WIT the bindings read, from the generated tokens.
fn inline(generated: &str) -> String {
    let start = generated.find("inline : ").expect("an inline WIT") + "inline : ".len();
    let literal: syn::LitStr =
        syn::parse_str(generated[start..].split(" , world").next().unwrap()).unwrap();
    literal.value()
}

/// A directory holding the contract of an Orb that imports a WASI interface, as `cargo tembra
/// build` writes it: the world, and under `deps/` each given package at its path there, which is
/// its file, or a file in a directory of its own.
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
        let path = dir.join("deps").join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    }
    dir
}

/// The last of the sources as an encoded WIT package, as `wit_component::encode` writes it, with
/// the parts of the packages before it that it uses.
fn encoded(sources: &[&str]) -> Vec<u8> {
    let mut resolve = wit_parser::Resolve::default();
    let mut package = None;
    for source in sources {
        package = Some(resolve.push_str("encoded.wit", source).unwrap());
    }
    wit_component::encode(&resolve, package.unwrap(), false).unwrap()
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
    // No `path` option, which would open the bindings' options with a consumed directory.
    assert!(!generated.contains("({ path :"), "{generated}");
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

/// A WASI interface is imported when a host serves it as an import, by the rule the installation's
/// grant check applies: `wasi:cli/run` and `wasi:http/handler`, which a component exports instead,
/// are errors on the import naming what the hosts serve of their package; an interface of types
/// only, `wasi:clocks/types`, asks nothing of a host.
#[test]
fn a_wasi_import_is_an_interface_the_hosts_serve() {
    for (identity, served) in [
        (
            "wasi:cli/run@0.3.0",
            "of wasi:cli@0.3.0, the hosts serve environment, exit, types, stdin, stdout, stderr, terminal-input, terminal-output, terminal-stdin, terminal-stdout, terminal-stderr",
        ),
        (
            "wasi:http/handler@0.3.0",
            "of wasi:http@0.3.0, the hosts serve types, client",
        ),
    ] {
        let error = expanded(importing(&[identity], None), DICE)
            .map(|_| ())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(&format!(
                "no host serves {identity} as an import: a component exports it instead"
            )) && error.contains(served),
            "{identity}: {error}"
        );
    }
    let generated = expanded(importing(&["wasi:clocks/types@0.3.0"], None), DICE)
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(
        generated.contains("import wasi:clocks/types@0.3.0;"),
        "{generated}"
    );
}

/// The Orb module re-exports each imported interface's module under the interface's name: two
/// WASI interfaces of one name, `wasi:clocks/types` and `wasi:http/types`, are an error naming
/// both, and either alone expands.
#[test]
fn two_imports_of_one_interface_name_are_refused() {
    let both = ["wasi:clocks/types@0.3.0", "wasi:http/types@0.3.0"];
    let error = expanded(importing(&both, None), DICE)
        .map(|_| ())
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("the Orb module would re-export two modules as `types`: that of the WASI interface wasi:http/types@0.3.0, and that of the WASI interface wasi:clocks/types@0.3.0; keep one of them"),
        "{error}"
    );
    for identity in both {
        expanded(importing(&[identity], None), DICE)
            .unwrap_or_else(|error| panic!("{identity}: {error}"));
    }
}

/// The bindings read the Orb's package beside the bundled packages the inline WIT nests, and one
/// WIT holds one package of a name: an Orb's package named like one of them, a WASI package an
/// import builds on or the kernel's package a state uses, is an error on the package, whether the
/// macro reads the bindings' types, as for a state, or not. The same modules under a package of
/// their own expand. An Orb's package named like a bundled package the expansion does not nest
/// passes the bindings, and is refused where the derived WIT resolves as a contract, beside the
/// bundled packages, for the signature of each service.
#[test]
fn an_orb_package_named_like_a_nested_package_is_refused() {
    let named = |package: &str, imports: &[&str]| Options {
        package: Some(literal(package)),
        ..importing(imports, None)
    };
    let filesystem: &[&str] = &["wasi:filesystem/types@0.3.0"];
    let wasi = "the Orb's package cannot be named wasi:clocks@0.3.0: the bindings read the WASI package Tembra serves of that name beside it, for the WASI imports; give the Orb a package name of its own";
    let kernel = "the Orb's package cannot be named tembra:guest@1.0.0: the bindings read the kernel's package of that name beside it, for a state, a resource or the context; give the Orb a package name of its own";
    for (package, imports, module, message) in [
        ("wasi:clocks@0.3.0", filesystem, DICE_WITH_STATE, wasi),
        ("wasi:clocks@0.3.0", filesystem, DICE, wasi),
        ("tembra:guest@1.0.0", &[][..], DICE_WITH_STATE, kernel),
    ] {
        let error = expanded(named(package, imports), module)
            .map(|_| ())
            .unwrap_err()
            .to_string();
        assert!(error.contains(message), "{package}, {imports:?}: {error}");
        expanded(named("example:dice@1.0.0", imports), module)
            .unwrap_or_else(|error| panic!("{imports:?}: {error}"));
    }
    for (package, imports, module) in [
        ("wasi:clocks@0.3.0", &[][..], DICE_WITH_STATE),
        ("wasi:http@0.3.0", filesystem, DICE_WITH_STATE),
        ("tembra:guest@1.0.0", filesystem, DICE),
    ] {
        let error = expanded(named(package, imports), module)
            .map(|_| ())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("the derived WIT does not resolve as a contract")
                && error.contains(package),
            "{package}, {imports:?}: {error}"
        );
    }
}

/// The bindings read the Orb's package beside the consumed packages too, and a copy of a bundled
/// package the consumed directory holds, which the inline WIT leaves out, is one of them: an Orb's
/// package named like such a copy, a WASI package's, whole or encoded, or the kernel's, is an error
/// on the package, as when the inline WIT nests that package, whether the macro reads the
/// bindings' types, as for a state, or not. The same modules under a package of their own expand.
#[test]
fn an_orb_package_named_like_a_copy_the_consumed_directory_holds_is_refused() {
    let clocks = bundled("wasi-clocks.wit");
    let guest = tembra_contracts::guest::SOURCE;
    let wasi = "the Orb's package cannot be named wasi:clocks@0.3.0: the consumed directory holds a copy of the WASI package Tembra serves of that name, which the bindings read beside it; give the Orb a package name of its own";
    let kernel = "the Orb's package cannot be named tembra:guest@1.0.0: the consumed directory holds a copy of the kernel's package of that name, which the bindings read beside it; give the Orb a package name of its own";
    let encoded_clocks = encoded(&[clocks]);
    let whole_copy: &[(&str, &[u8])] = &[("wasi-clocks.wit", clocks.as_bytes())];
    let encoded_copy: &[(&str, &[u8])] = &[("wasi-clocks.wasm", &encoded_clocks)];
    let kernel_copy: &[(&str, &[u8])] = &[
        ("wasi-clocks.wit", clocks.as_bytes()),
        ("tembra-guest.wit", guest.as_bytes()),
    ];
    for (name, deps, package, module, message) in [
        (
            "whole",
            whole_copy,
            "wasi:clocks@0.3.0",
            DICE_WITH_STATE,
            wasi,
        ),
        (
            "whole-stateless",
            whole_copy,
            "wasi:clocks@0.3.0",
            DICE,
            wasi,
        ),
        (
            "encoded",
            encoded_copy,
            "wasi:clocks@0.3.0",
            DICE_WITH_STATE,
            wasi,
        ),
        (
            "kernel",
            kernel_copy,
            "tembra:guest@1.0.0",
            DICE_WITH_STATE,
            kernel,
        ),
    ] {
        let dir = contract(&format!("named-like-{name}"), &[]);
        for (file, copy) in deps {
            std::fs::write(dir.join("deps").join(file), copy).unwrap();
        }
        let named = |package: &str| Options {
            package: Some(literal(package)),
            ..importing(&["example:timer/timer@1.0.0"], Some(&dir))
        };
        let error = expanded(named(package), module)
            .map(|_| ())
            .unwrap_err()
            .to_string();
        assert!(error.contains(message), "{name}: {error}");
        expanded(named("example:dice@1.0.0"), module)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}

/// A contract `cargo tembra build` wrote for an Orb that imports WASI holds the WASI packages in
/// `deps/`: an Orb consuming it reads each such package once, from there, the bundled copy left out
/// of its inline WIT, and nests only the WASI packages the directory lacks. A copy is the bundled
/// file line for line, whatever its line endings, as `Contract::from_dir` reads it; a copy that
/// differs, if only in its comments, is an error naming its file.
#[test]
fn a_consumed_contract_may_bring_the_wasi_packages() {
    let clocks = bundled("wasi-clocks.wit");
    let imports = [
        "example:timer/timer@1.0.0",
        "wasi:clocks/monotonic-clock@0.3.0",
        "wasi:random/random@0.3.0",
    ];
    let checked_out = clocks.replace('\n', "\r\n");
    for (name, copy) in [("deps", clocks), ("line-endings", checked_out.as_str())] {
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
    let commented = clocks.replace("/// ", "/// (another text) ");
    let different = clocks.replacen(
        "get-resolution: func() -> duration;",
        "get-resolution: func() -> u32;",
        1,
    );
    for (name, copy) in [("comments", &commented), ("different", &different)] {
        let dir = contract(name, &[("wasi-clocks.wit", copy)]);
        let error = expanded(importing(&imports, Some(&dir)), DICE)
            .map(|_| ())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("WIT package wasi:clocks@0.3.0 in ")
                && error.contains("deps/wasi-clocks.wit is already provided by the standard contracts bundled with Tembra; remove the duplicate"),
            "{name}: {error}"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}

/// Every copy of a bundled package a consumed directory holds is checked as `Contract::from_dir`
/// checks it, whether the imports need the package or not: an edited copy in `deps/` of
/// `wasi:random` or of the kernel's package, in an Orb that imports `wasi:clocks` or no WASI
/// interface and has no state, is an error naming its file and what the package is, a copy in a
/// directory of `deps/`, even line for line, or nested in a file of the directory itself, is
/// `from_dir`'s error naming that directory, and a copy nested in a file of `deps/` that holds
/// another package, even line for line, is `from_dir`'s error naming that file, a comment on that
/// file's `package` line included. The same copies, line for line in `deps/` itself, expand, and
/// so does that file without the nested copy.
#[test]
fn every_copy_of_a_bundled_package_is_checked() {
    let clocks = bundled("wasi-clocks.wit");
    let random = bundled("wasi-random.wit");
    let guest = tembra_contracts::guest::SOURCE;
    let imports: [&[&str]; 2] = [
        &[
            "example:timer/timer@1.0.0",
            "wasi:clocks/monotonic-clock@0.3.0",
        ],
        &["example:timer/timer@1.0.0"],
    ];
    let refused = |imports: &[&str], dir: &Path| {
        expanded(importing(imports, Some(dir)), DICE)
            .map(|_| ())
            .unwrap_err()
            .to_string()
    };
    let provided = |package: &str, place: &Path| {
        format!(
            "WIT package {package} in {} is already provided by the standard contracts bundled with Tembra; remove the duplicate",
            place.display()
        )
    };
    let dir = contract(
        "unused-copies",
        &[
            ("wasi-clocks.wit", clocks),
            ("wasi-random.wit", random),
            ("tembra-guest.wit", guest),
        ],
    );
    for imports in imports {
        expanded(importing(imports, Some(&dir)), DICE)
            .unwrap_or_else(|error| panic!("{imports:?}: {error}"));
    }
    std::fs::remove_dir_all(dir).unwrap();
    let edited_random = random.replace("/// ", "/// (another text) ");
    let edited_guest = guest.replace("/// ", "/// (another text) ");
    for (file, copy, package) in [
        (
            "wasi-random.wit",
            edited_random.as_str(),
            "wasi:random@0.3.0",
        ),
        (
            "tembra-guest.wit",
            edited_guest.as_str(),
            "tembra:guest@1.0.0",
        ),
    ] {
        let dir = contract(
            &format!("edited-{file}"),
            &[("wasi-clocks.wit", clocks), (file, copy)],
        );
        for imports in imports {
            let error = refused(imports, &dir);
            assert!(
                error.contains(&provided(package, &dir.join("deps").join(file))),
                "{imports:?}: {error}"
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
    for (name, deps, package, place) in [
        (
            "clocks-directory",
            vec![("clocks/wasi-clocks.wit", clocks)],
            "wasi:clocks@0.3.0",
            "clocks",
        ),
        (
            "kernel-directory",
            vec![
                ("wasi-clocks.wit", clocks),
                ("kernel/tembra-guest.wit", guest),
            ],
            "tembra:guest@1.0.0",
            "kernel",
        ),
    ] {
        let dir = contract(name, &deps);
        let message = provided(package, &dir.join("deps").join(place));
        for imports in imports {
            let error = refused(imports, &dir);
            assert!(error.contains(&message), "{name}, {imports:?}: {error}");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
    let dir = contract("nested-clocks", &[]);
    let timer = dir.join("timer.wit");
    let own = std::fs::read_to_string(&timer).unwrap();
    let nested = crate::wit::nested(clocks).unwrap();
    std::fs::write(&timer, format!("{own}\n{nested}")).unwrap();
    let message = provided("wasi:clocks@0.3.0", &dir);
    for imports in imports {
        let error = refused(imports, &dir);
        assert!(error.contains(&message), "{imports:?}: {error}");
    }
    std::fs::remove_dir_all(dir).unwrap();
    let book = "\n\ninterface book {\n  total: func() -> u32;\n}\n";
    for (line, header) in [
        ("plain", "package example:ledger@1.0.0;"),
        ("commented", "package example:ledger@1.0.0; // the ledger"),
    ] {
        let ledger = format!("{header}{book}");
        for (name, copy) in [("identical", random), ("edited", edited_random.as_str())] {
            let dir = contract(
                &format!("nested-random-{line}-{name}"),
                &[("wasi-clocks.wit", clocks), ("ledger.wit", &ledger)],
            );
            for imports in imports {
                expanded(importing(imports, Some(&dir)), DICE)
                    .unwrap_or_else(|error| panic!("{line} {name}, {imports:?}: {error}"));
            }
            let file = dir.join("deps").join("ledger.wit");
            let nested = crate::wit::nested(copy).unwrap();
            std::fs::write(&file, format!("{ledger}\n{nested}")).unwrap();
            let message = provided("wasi:random@0.3.0", &file);
            for imports in imports {
                let error = refused(imports, &dir);
                assert!(
                    error.contains(&message),
                    "{line} {name}, {imports:?}: {error}"
                );
            }
            std::fs::remove_dir_all(dir).unwrap();
        }
    }
}

/// A bundled package in `deps/` encoded as a WIT package, as `wit_component::encode` writes it,
/// has no source to compare line for line: `Contract::from_dir` merges each encoded file into the
/// bundled packages. An exact copy merges, so it is a copy whether the Orb imports its package or
/// not: the inline WIT leaves the bundled one out and resolves after the consumed directory, as
/// the bindings read them. A copy the merge refuses is `from_dir`'s error naming its file.
#[test]
fn an_encoded_copy_of_a_bundled_package_is_merged_as_from_dir_merges_it() {
    let clocks = bundled("wasi-clocks.wit");
    let imports: [&[&str]; 2] = [
        &[
            "example:timer/timer@1.0.0",
            "wasi:clocks/monotonic-clock@0.3.0",
        ],
        &["example:timer/timer@1.0.0"],
    ];
    let dir = contract("encoded", &[]);
    std::fs::write(
        dir.join("deps").join("wasi-clocks.wasm"),
        encoded(&[clocks]),
    )
    .unwrap();
    for imports in imports {
        let generated = expanded(importing(imports, Some(&dir)), DICE)
            .unwrap_or_else(|error| panic!("{imports:?}: {error}"));
        let inline = inline(&generated);
        assert!(
            !inline.contains("package wasi:clocks@0.3.0"),
            "{imports:?}: {inline}"
        );
        assert!(
            generated.contains("pub use self :: example :: timer :: timer ;"),
            "{generated}"
        );
        let mut consumed = wit_parser::Resolve::default();
        consumed.push_dir(&dir).unwrap();
        super::tests::resolved_with(consumed, &inline);
    }
    std::fs::remove_dir_all(dir).unwrap();
    let different = clocks.replacen(
        "get-resolution: func() -> duration;",
        "get-resolution: func() -> u32;",
        1,
    );
    let dir = contract("encoded-different", &[]);
    let file = dir.join("deps").join("wasi-clocks.wasm");
    std::fs::write(&file, encoded(&[different.as_str()])).unwrap();
    let message = format!(
        "failed to parse WIT package {}: failed to merge package `wasi:clocks@0.3.0` into existing copy",
        file.display()
    );
    for imports in imports {
        let error = expanded(importing(imports, Some(&dir)), DICE)
            .map(|_| ())
            .unwrap_err()
            .to_string();
        assert!(error.contains(&message), "{imports:?}: {error}");
    }
    std::fs::remove_dir_all(dir).unwrap();
}

/// The kernel's package the macro brings itself, for a state, a resource or the context, nested in
/// the inline WIT unless the consumed directory holds it whole: a copy of it there decoded from an
/// encoded file, an exact one or the part another encoded package uses, is then an error asking
/// to remove it, and the same modules expand without it. An Orb with none of them nests no kernel
/// package, and its bindings read that copy from the consumed directory, as `Contract::from_dir`
/// accepts it: a stateless Orb consuming `example:timer`, whose `now` returns the `width` of
/// `deps/span.wasm`, which `use`s the kernel's `level`, expands, and its inline WIT resolves after
/// the directory.
#[test]
fn an_encoded_copy_of_the_kernel_package_is_refused_where_the_macro_brings_it() {
    let clocks = bundled("wasi-clocks.wit");
    let guest = tembra_contracts::guest::SOURCE;
    let span = "package example:span@1.0.0;\n\ninterface span {\n  use tembra:guest/context@1.0.0.{level};\n  type width = level;\n}\n";
    let imports: [&[&str]; 2] = [
        &[
            "example:timer/timer@1.0.0",
            "wasi:clocks/monotonic-clock@0.3.0",
        ],
        &["example:timer/timer@1.0.0"],
    ];
    let with_resource = "mod dice {
        #[resource]
        pub struct Cup { pub size: u32 }
        impl Cup {
            #[open]
            pub fn open() -> Cup { Cup { size: 2 } }
        }
        pub mod chance {
            #[service]
            pub struct Chance;
            impl Chance {
                pub fn draw(length: u64) -> Vec<u8> { super::random::get_random_bytes(length) }
            }
        }
    }";
    // The modules whose bindings read the kernel's package, by what reads it.
    let kernel = |imports: &[&str], dir: &Path| {
        [
            ("a state", importing(imports, Some(dir)), DICE_WITH_STATE),
            ("a resource", importing(imports, Some(dir)), with_resource),
            (
                "the context",
                Options {
                    context: true,
                    ..importing(imports, Some(dir))
                },
                DICE,
            ),
        ]
    };
    for (name, sources) in [("tembra-guest", vec![guest]), ("span", vec![guest, span])] {
        let dir = contract(&format!("encoded-{name}"), &[("wasi-clocks.wit", clocks)]);
        let file = dir.join("deps").join(format!("{name}.wasm"));
        std::fs::write(&file, encoded(&sources)).unwrap();
        let timer = dir.join("timer.wit");
        let own = std::fs::read_to_string(&timer).unwrap();
        if name == "span" {
            let uses = own.replace(
                "  now: func() -> u64;",
                "  use example:span/span@1.0.0.{width};\n  now: func() -> width;",
            );
            std::fs::write(&timer, uses).unwrap();
        }
        let message = format!(
            "{}: this copy of tembra:guest@1.0.0, the kernel's package, is encoded; remove it, since the macro brings the kernel's package",
            file.display()
        );
        for imports in imports {
            let generated = expanded(importing(imports, Some(&dir)), DICE)
                .unwrap_or_else(|error| panic!("{name}, {imports:?}: {error}"));
            let inline = inline(&generated);
            assert!(
                !inline.contains("package tembra:guest"),
                "{name}, {imports:?}: {inline}"
            );
            let mut consumed = wit_parser::Resolve::default();
            consumed.push_dir(&dir).unwrap();
            super::tests::resolved_with(consumed, &inline);
            for (what, options, module) in kernel(imports, &dir) {
                let error = expanded(options, module)
                    .map(|_| ())
                    .unwrap_err()
                    .to_string();
                assert!(
                    error.contains(&message),
                    "{name}, {what}, {imports:?}: {error}"
                );
            }
        }
        std::fs::remove_file(&file).unwrap();
        std::fs::write(&timer, &own).unwrap();
        for imports in imports {
            for (what, options, module) in kernel(imports, &dir) {
                expanded(options, module)
                    .unwrap_or_else(|error| panic!("{name}, {what}, {imports:?}: {error}"));
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}

/// An encoded package holds only the parts of the bundled packages it uses, and the bindings read
/// those packages from the consumed directory: `deps/span.wasm`, whose `span` uses a type of
/// `wasi:clocks`, brings a copy of `wasi:clocks` holding that type and what it builds on. A WASI
/// import that needs a part the copy lacks is refused on its literal, naming the copy, the file it
/// was decoded from and what it lacks: the imported interface, a function of it, or an interface a
/// bundled package nested for the import uses. The same directory expands without the import.
#[test]
fn an_encoded_copy_must_hold_what_a_wasi_import_needs() {
    let clocks = bundled("wasi-clocks.wit");
    let timer = "package example:timer@1.0.0;\n\ninterface timer {\n  use example:span/span@1.0.0.{width};\n  now: func() -> width;\n}\n\nworld timer-world {\n  export timer;\n}\n";
    for (name, used, import, lack) in [
        (
            "interface",
            "types@0.3.0.{duration}",
            "wasi:clocks/monotonic-clock@0.3.0",
            "wasi:clocks/monotonic-clock@0.3.0",
        ),
        (
            "function",
            "monotonic-clock@0.3.0.{mark}",
            "wasi:clocks/monotonic-clock@0.3.0",
            "the function now of wasi:clocks/monotonic-clock@0.3.0",
        ),
        (
            "nested",
            "types@0.3.0.{duration}",
            "wasi:filesystem/preopens@0.3.0",
            "wasi:clocks/system-clock@0.3.0, which wasi:filesystem@0.3.0 uses",
        ),
    ] {
        let ty = used.split(['{', '}']).nth(1).unwrap();
        let span = format!(
            "package example:span@1.0.0;\n\ninterface span {{\n  use wasi:clocks/{used};\n  type width = {ty};\n}}\n"
        );
        let mut resolve = wit_parser::Resolve::default();
        resolve.push_str("wasi-clocks.wit", clocks).unwrap();
        let package = resolve.push_str("span.wit", &span).unwrap();
        let dir = contract(&format!("partial-{name}"), &[]);
        std::fs::write(dir.join("timer.wit"), timer).unwrap();
        let file = dir.join("deps").join("span.wasm");
        std::fs::write(
            &file,
            wit_component::encode(&resolve, package, false).unwrap(),
        )
        .unwrap();
        let alone: &[&str] = &["example:timer/timer@1.0.0"];
        let generated = expanded(importing(alone, Some(&dir)), DICE)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        let mut consumed = wit_parser::Resolve::default();
        consumed.push_dir(&dir).unwrap();
        super::tests::resolved_with(consumed, &inline(&generated));
        let error = expanded(importing(&[alone[0], import], Some(&dir)), DICE)
            .map(|_| ())
            .unwrap_err()
            .to_string();
        let message = format!(
            "the consumed directory holds wasi:clocks@0.3.0 only in part, decoded from {}, without {lack}; the bindings read wasi:clocks@0.3.0 from there",
            file.display()
        );
        assert!(error.contains(&message), "{name}: {error}");
        std::fs::remove_dir_all(dir).unwrap();
    }
}

/// `Resolve::push_dir`, with which the macro and the bindings read the consumed directory, merges
/// each encoded file of `deps/` first, then adds the text packages, a `.wit` file or a directory
/// of `deps/` and the directory's own, asserting that each is new: a package held both ways, even
/// as the bundled file line for line beside an encoded package that holds part of it, made the
/// macro panic. It is an error naming both places, read before the directory, and the same
/// directory without the encoded file expands.
#[test]
fn a_package_held_as_text_and_encoded_is_refused() {
    let guest = tembra_contracts::guest::SOURCE;
    let clocks = bundled("wasi-clocks.wit");
    let span = "package example:span@1.0.0;\n\ninterface span {\n  type width = u32;\n}\n";
    let span_clocks = "package example:span@1.0.0;\n\ninterface span {\n  use wasi:clocks/types@0.3.0.{duration};\n  type width = duration;\n}\n";
    let timer = "package example:timer@1.0.0;\n\ninterface timer {\n  now: func() -> u64;\n}\n\nworld timer-world {\n  export timer;\n}\n";
    // Each directory's text packages in `deps/`, its encoded file and the sources it encodes, the
    // package held twice and where its text is, the directory itself when none is given.
    let layouts = [
        (
            "guest",
            vec![("tembra-guest.wit", guest)],
            "tembra-guest.wasm",
            vec![guest],
            "tembra:guest@1.0.0",
            "deps/tembra-guest.wit",
        ),
        (
            "span",
            vec![("span.wit", span)],
            "span.wasm",
            vec![span],
            "example:span@1.0.0",
            "deps/span.wit",
        ),
        (
            "clocks",
            vec![("wasi-clocks.wit", clocks)],
            "span.wasm",
            vec![clocks, span_clocks],
            "wasi:clocks@0.3.0",
            "deps/wasi-clocks.wit",
        ),
        (
            "span-directory",
            vec![("span/span.wit", span)],
            "span.wasm",
            vec![span],
            "example:span@1.0.0",
            "deps/span",
        ),
        (
            "timer",
            vec![],
            "timer.wasm",
            vec![timer],
            "example:timer@1.0.0",
            "",
        ),
    ];
    let imports: &[&str] = &["example:timer/timer@1.0.0"];
    for (name, text, file, sources, package, held) in layouts {
        let dir = contract(&format!("held-twice-{name}"), &text);
        std::fs::write(dir.join("timer.wit"), timer).unwrap();
        let file = dir.join("deps").join(file);
        std::fs::write(&file, encoded(&sources)).unwrap();
        let place = if held.is_empty() {
            dir.clone()
        } else {
            dir.join(held)
        };
        let message = format!(
            "WIT package {package} is held twice in the consumed directory, in {} and, encoded, in {}: the bindings cannot read a package held both ways; remove one of them",
            place.display(),
            file.display()
        );
        let error = expanded(importing(imports, Some(&dir)), DICE)
            .map(|_| ())
            .unwrap_err()
            .to_string();
        assert!(error.contains(&message), "{name}: {error}");
        std::fs::remove_file(&file).unwrap();
        expanded(importing(imports, Some(&dir)), DICE)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
