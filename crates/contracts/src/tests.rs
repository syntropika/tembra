use super::*;
use std::collections::BTreeMap;
use tembra_core::Host;

const SOURCE: &str = r#"
package example:calculation@1.2.0;
interface arithmetic {
    record input { left: s32, right: s32 }
    add: func(value: input) -> s32;
    sum: async func(values: list<s32>) -> s64;
}
interface calculation { calculate: async func() -> s32; }
world provider { export arithmetic; }
world consumer { import arithmetic; export calculation; }
"#;

fn bindings() -> BTreeMap<String, Binding> {
    BTreeMap::from([(
        "example:calculation/arithmetic@1.2.0".into(),
        Binding::Orb {
            name: "calculator".into(),
            api_version: 7,
        },
    )])
}

#[test]
fn resolved_contracts_drive_kernel_admission() {
    let provider = Contract::from_source("example.wit", SOURCE, Some("provider")).unwrap();
    let consumer = Contract::from_source("example.wit", SOURCE, Some("consumer")).unwrap();
    let provider = provider.plan("calculator", 7, &BTreeMap::new()).unwrap();
    let consumer = consumer.plan("assistant", 2, &bindings()).unwrap();
    assert_eq!(consumer.orb.requires[0].orb, "calculator");
    assert_eq!(consumer.orb.requires[0].api_version, 7);
    assert!(consumer.host_imports.is_empty());
    let host = Host::new();
    let ticket = host
        .prepare(vec![provider.orb.clone(), consumer.orb.clone()], vec![])
        .unwrap();
    host.commit(ticket.id, vec![]).unwrap();
    let mut bad = provider.orb;
    bad.services[0].methods.remove("sum");
    assert!(
        Host::new()
            .prepare(vec![bad, consumer.orb], vec![])
            .is_err()
    );
}

#[test]
fn identity_and_orb_version_are_not_wit_versions() {
    let contract = Contract::from_source("example.wit", SOURCE, Some("provider")).unwrap();
    let a = contract.plan("first", 3, &BTreeMap::new()).unwrap();
    let b = contract.plan("second", 9, &BTreeMap::new()).unwrap();
    assert_ne!(a.orb.services[0].name, b.orb.services[0].name);
    assert_eq!(a.orb.api_version, 3);
    assert_eq!(b.orb.api_version, 9);
    assert_eq!(contract.describe()["package"], "example:calculation@1.2.0");
    assert!(contract.plan("", 1, &BTreeMap::new()).is_err());
}

#[test]
fn provider_selection_is_explicit_and_checked() {
    let contract = Contract::from_source("example.wit", SOURCE, Some("consumer")).unwrap();
    assert!(contract.plan("consumer", 1, &BTreeMap::new()).is_err());
    let plan = contract.plan("consumer", 1, &bindings()).unwrap();
    let changed = SOURCE.replace("left: s32", "left: string");
    let provider = Contract::from_source("changed.wit", &changed, Some("provider"))
        .unwrap()
        .plan("calculator", 7, &BTreeMap::new())
        .unwrap();
    let error = Host::new()
        .prepare(vec![provider.orb, plan.orb], vec![])
        .unwrap_err();
    assert!(error.message.contains("signature differs"));
}

#[test]
fn a_live_reference_prevents_replacement_with_a_different_wit_abi() {
    let contract = Contract::from_source("example.wit", SOURCE, Some("provider")).unwrap();
    let host = Host::new();
    let installed = host
        .prepare(
            vec![
                contract
                    .plan("calculator", 7, &BTreeMap::new())
                    .unwrap()
                    .orb,
            ],
            vec![],
        )
        .unwrap();
    host.commit(installed.id, vec![]).unwrap();
    let claim = host
        .acquire(
            tembra_core::Address {
                service: "calculator::example:calculation/arithmetic@1.2.0".into(),
                key: vec!["main".into()],
                principal: "host".into(),
            },
            7,
            std::collections::BTreeSet::from(["add".into()]),
        )
        .unwrap();
    host.ready(claim.object).unwrap();
    let changed = Contract::from_source(
        "changed.wit",
        &SOURCE.replace("left: s32", "left: string"),
        Some("provider"),
    )
    .unwrap();
    let replacement = changed.plan("calculator", 7, &BTreeMap::new()).unwrap().orb;
    assert_eq!(
        host.prepare(vec![replacement.clone()], vec![])
            .unwrap_err()
            .reason,
        "dependency_service"
    );
    assert_eq!(host.version(), installed.code_version);
    host.release(claim.reference).unwrap();
    assert!(host.prepare(vec![replacement], vec![]).is_ok());
}

#[test]
fn resource_imports_require_a_provider_even_without_functions() {
    let source = "package example:handles; interface types { resource handle; } world input { import types; } world output { export types; }";
    let input = Contract::from_source("resource.wit", source, Some("input")).unwrap();
    assert_eq!(input.describe()["imports"][0]["type_only"], false);
    assert!(input.plan("input", 1, &BTreeMap::new()).is_err());
    let bindings = BTreeMap::from([(
        "example:handles/types".into(),
        Binding::Host {
            provider: "storage".into(),
        },
    )]);
    let plan = input.plan("input", 1, &bindings).unwrap();
    assert_eq!(plan.host_imports[0].provider, "storage");
    assert!(plan.orb.requires.is_empty());
    let output = Contract::from_source("resource.wit", source, Some("output")).unwrap();
    // Type declarations can be prepared without inventing a callable service or lifetime.
    assert!(
        output
            .plan("output", 1, &BTreeMap::new())
            .unwrap()
            .orb
            .services
            .is_empty()
    );
    assert!(output.check_value_profile().is_err());
}

#[test]
fn rich_types_and_async_signatures_survive_preparation() {
    let source = "package example:rich; interface values { resource file; record value { body: stream<u8>, done: future<u32> } record unused { flag: bool } use-file: async func(f: borrow<file>, v: value) -> own<file>; } world test { export values; }";
    let contract = Contract::from_source("rich.wit", source, None).unwrap();
    let description = contract.describe().to_string();
    for expected in ["stream", "future", "borrow", "own", "resource", "async"] {
        assert!(description.contains(expected), "missing {expected}");
    }
    // Only definitions reachable from the world's signatures are carried at runtime.
    assert!(!description.contains("\"flag\""));
    let decoded = PreparedContract::decode(&contract.prepared().encode()).unwrap();
    assert_eq!(&decoded, contract.prepared());
    assert_eq!(decoded.describe(), contract.describe());
    assert!(decoded.check_value_profile().is_err());
}

#[test]
fn malformed_ambiguous_and_unused_input_is_rejected() {
    assert!(Contract::from_source("bad.wit", "not WIT", None).is_err());
    assert!(
        Contract::from_source(
            "two.wit",
            "package example:two; world one {} world two {}",
            None
        )
        .is_err()
    );
    let provider = Contract::from_source("example.wit", SOURCE, Some("provider")).unwrap();
    assert!(provider.plan("provider", 1, &bindings()).is_err());
}

#[test]
fn value_validation_checks_records_widths_and_tuple_lengths() {
    let contract = Contract::from_source("example.wit", SOURCE, Some("provider")).unwrap();
    let add = contract
        .bind("example:calculation/arithmetic@1.2.0", "add", true)
        .unwrap();
    let arguments = |value| contract.validate_slot(add, true, &value);
    assert!(arguments(serde_json::json!([{"left":1,"right":2}])).is_ok());
    assert!(arguments(serde_json::json!([{"left":2147483648_u64,"right":2}])).is_err());
    assert!(arguments(serde_json::json!([{"left":1,"right":2,"extra":3}])).is_err());
    assert!(arguments(serde_json::json!([])).is_err());
    assert!(
        contract
            .validate_slot(add, false, &serde_json::json!(3))
            .is_ok()
    );
    assert!(
        contract
            .validate_slot(add, false, &serde_json::json!("3"))
            .is_err()
    );
    // `sum` returns an `s64`: 64-bit integers are part of the portable profile.
    assert!(contract.check_value_profile().is_ok());
}

#[test]
fn standard_contracts_are_managed_and_encoded_with_the_package() {
    let source = "package example:standard; world run { import wasi:filesystem/preopens@0.3.0; }";
    let contract = Contract::from_source("standard.wit", source, None).unwrap();
    let description = contract.describe();
    assert!(
        description["imports"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["contract"] == "wasi:filesystem/preopens@0.3.0")
    );
    assert!(contract.encode().unwrap().starts_with(b"\0asm"));
    assert!(contract.plan("storage", 1, &BTreeMap::new()).is_err());
    // WASI resources, handles, streams and futures survive the compact form unchanged.
    let compact = contract.prepared().encode();
    let decoded = PreparedContract::decode(&compact).unwrap();
    assert_eq!(&decoded, contract.prepared());
    assert_eq!(decoded.describe(), description);
    assert!(compact.len() < contract.encode().unwrap().len());
    let grants = Grants {
        filesystem: vec![FilesystemGrant {
            provider: "files".into(),
            mount: "/data".into(),
            writable: false,
        }],
        ..Grants::default()
    };
    let bindings = decoded.providers(&BTreeMap::new(), &grants).unwrap();
    assert_eq!(
        bindings,
        contract.providers(&BTreeMap::new(), &grants).unwrap()
    );
    let plan = decoded.plan("storage", 1, &bindings).unwrap();
    assert!(
        plan.host_imports
            .iter()
            .any(|import| import.contract == "wasi:filesystem/preopens@0.3.0")
    );
}

/// A contract that imports a nested package, whose world uses the kernel's state, and the WASI
/// filesystem, which builds on the clocks.
const RELAY: &str = "package example:relay@1.0.0;

interface relaying {
  twice: func() -> u32;
}

world relay {
  import example:tally/tally@1.0.0;
  import wasi:filesystem/types@0.3.0;
  export relaying;
}

package example:tally@1.0.0 {
  interface tally {
    bump: func() -> u32;
  }
  world counter {
    import tembra:guest/state@1.0.0;
    export tembra:guest/state-init@1.0.0;
    export tally;
  }
}
";

/// A contract lists the bundled standard contracts it depends on, through any of its packages and
/// through each other, in dependency order, so that its directory resolves with them alone; a
/// contract without any lists none.
#[test]
fn a_contract_lists_the_standard_contracts_it_depends_on() {
    let contract = Contract::from_source("relay.wit", RELAY, Some("relay")).unwrap();
    let files: Vec<&str> = contract
        .standard_dependencies()
        .into_iter()
        .map(|(file, _)| file)
        .collect();
    assert_eq!(
        files,
        ["wasi-clocks.wit", "wasi-filesystem.wit", "tembra-guest.wit"]
    );
    let plain = Contract::from_source("example.wit", SOURCE, Some("consumer")).unwrap();
    assert!(plain.standard_dependencies().is_empty());
    // The worlds of WASI HTTP import the CLI, whose worlds import the rest of the release.
    let http = "package example:fetch@1.0.0;\n\ninterface fetching {\n  fetch: func(url: string) -> string;\n}\n\n\
                world fetch {\n  import wasi:http/types@0.3.0;\n  export fetching;\n}\n";
    let contract = Contract::from_source("fetch.wit", http, Some("fetch")).unwrap();
    let dependencies = contract.standard_dependencies();
    assert_eq!(
        dependencies
            .iter()
            .map(|(file, _)| *file)
            .collect::<Vec<_>>(),
        [
            "wasi-clocks.wit",
            "wasi-random.wit",
            "wasi-filesystem.wit",
            "wasi-sockets.wit",
            "wasi-cli.wit",
            "wasi-http.wit"
        ]
    );
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("fetch.wit"), http).unwrap();
    std::fs::create_dir(root.path().join("deps")).unwrap();
    for (file, source) in dependencies {
        std::fs::write(root.path().join("deps").join(file), source).unwrap();
    }
    wit_parser::Resolve::default()
        .push_dir(root.path())
        .unwrap();
    let read = Contract::from_dir(root.path(), Some("fetch")).unwrap();
    assert_eq!(read.prepared(), contract.prepared());
}

/// With the standard contracts it depends on in `deps/`, a contract directory resolves on its own,
/// as another Orb's bindings read it with `Resolve::push_dir`, and `Contract::from_dir` reads it as
/// the contract itself: it skips an exact copy of a bundled contract, whatever its line endings,
/// and refuses any other definition of a bundled package.
#[test]
fn a_contract_directory_resolves_alone_with_its_standard_contracts() {
    let contract = Contract::from_source("relay.wit", RELAY, Some("relay")).unwrap();
    let directory = |line_ending: &str, note: &str| {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("relay.wit"), RELAY).unwrap();
        std::fs::create_dir(root.path().join("deps")).unwrap();
        for (file, source) in contract.standard_dependencies() {
            let source = format!("{source}{note}").replace('\n', line_ending);
            std::fs::write(root.path().join("deps").join(file), source).unwrap();
        }
        root
    };
    for line_ending in ["\n", "\r\n"] {
        let root = directory(line_ending, "");
        wit_parser::Resolve::default()
            .push_dir(root.path())
            .unwrap();
        let read = Contract::from_dir(root.path(), Some("relay")).unwrap();
        assert_eq!(read.prepared(), contract.prepared());
    }
    let changed = directory("\n", "// A local change.\n");
    wit_parser::Resolve::default()
        .push_dir(changed.path())
        .unwrap();
    let error = Contract::from_dir(changed.path(), Some("relay"))
        .err()
        .unwrap();
    assert!(
        error
            .0
            .contains("already provided by the standard contracts bundled"),
        "{error}"
    );
    // Without them, the directory names packages it does not hold.
    let alone = tempfile::tempdir().unwrap();
    std::fs::write(alone.path().join("relay.wit"), RELAY).unwrap();
    assert!(
        wit_parser::Resolve::default()
            .push_dir(alone.path())
            .is_err()
    );
}

#[test]
fn lossy_values_are_refused_before_the_interpreter_runs() {
    let nested = Contract::from_source("nested.wit","package example:values; interface values { type maybe = option<u32>; read: func() -> option<maybe>; } world run { export values; }",None).unwrap();
    assert!(
        nested
            .check_value_profile()
            .unwrap_err()
            .0
            .contains("tagged option")
    );
    let float = Contract::from_source("float.wit","package example:values; interface values { read: func() -> f32; } world run { export values; }",None).unwrap();
    let read = float.bind("example:values/values", "read", true).unwrap();
    assert!(
        float
            .validate_slot(read, false, &serde_json::json!(-0.0))
            .is_err()
    );
    // An Orb import with 64-bit integers is admitted; error contexts still have no codec.
    let consumer = Contract::from_source("example.wit", SOURCE, Some("consumer")).unwrap();
    assert!(
        consumer
            .check_import_value_profile("example:calculation/arithmetic@1.2.0")
            .is_ok()
    );
    let context = Contract::from_source("context.wit","package example:values; interface values { read: func() -> error-context; } world run { export values; }",None).unwrap();
    assert!(
        context
            .check_value_profile()
            .unwrap_err()
            .0
            .contains("error contexts")
    );
}

#[test]
fn executable_export_comparison_checks_signatures_and_function_sets() {
    let expected = Contract::from_source("expected.wit", SOURCE, Some("provider")).unwrap();
    let matching = Contract::from_bytes(&expected.encode().unwrap(), Some("provider")).unwrap();
    expected.check_exports(&matching).unwrap();
    let changed = SOURCE.replace("left: s32", "left: string");
    let changed = Contract::from_source("changed.wit", &changed, Some("provider")).unwrap();
    assert!(expected.check_exports(&changed).is_err());
    let other = Contract::from_source("other.wit", SOURCE, Some("consumer")).unwrap();
    assert!(expected.check_exports(&other).is_err());
}

const STATEFUL: &str = r#"
package example:counter@1.0.0;
interface counter { add: func(by: s64) -> s64; }
world stateful {
    import tembra:guest/state@1.0.0;
    export tembra:guest/state-init@1.0.0;
    import tembra:guest/resources@1.0.0;
    export tembra:guest/resources-init@1.0.0;
    export counter;
}
"#;

#[test]
fn kernel_interfaces_bind_to_the_kernel_without_a_provider_or_grant() {
    let contract = Contract::from_source("counter.wit", STATEFUL, None).unwrap();
    let bindings = contract
        .providers(&BTreeMap::new(), &Grants::default())
        .unwrap();
    assert_eq!(bindings.get(guest::STATE), Some(&Binding::Kernel));
    assert_eq!(bindings.get(guest::RESOURCE), Some(&Binding::Kernel));
    let plan = contract.plan("counter", 1, &bindings).unwrap();
    assert!(plan.host_imports.is_empty());
    assert!(plan.orb.requires.is_empty());
    // The state-init export answers the kernel; only the Orb's own interface is a service.
    assert_eq!(
        plan.orb
            .services
            .iter()
            .map(|service| service.name.as_str())
            .collect::<Vec<_>>(),
        ["counter::example:counter/counter@1.0.0"]
    );
    // A kernel binding given by hand is checked like any other.
    let wrong: BTreeMap<String, Binding> = [(
        guest::STATE.to_owned(),
        Binding::Orb {
            name: "kernel".into(),
            api_version: 1,
        },
    )]
    .into();
    assert!(contract.plan("counter", 1, &wrong).is_err());

    // The import and the export come together, for each kernel interface.
    for (export, name) in [
        ("    export tembra:guest/state-init@1.0.0;\n", "state-init"),
        (
            "    export tembra:guest/resources-init@1.0.0;\n",
            "resources-init",
        ),
    ] {
        let half = STATEFUL.replace(export, "");
        let contract = Contract::from_source("half.wit", &half, None).unwrap();
        let bindings = contract
            .providers(&BTreeMap::new(), &Grants::default())
            .unwrap();
        let error = contract.plan("counter", 1, &bindings).unwrap_err();
        assert!(error.0.contains(name), "{error}");
    }

    // The namespace is reserved: other kernel interfaces are refused, and the package cannot
    // be redefined by an Orb.
    let reversed = STATEFUL
        .replace(
            "import tembra:guest/state@1.0.0;",
            "import tembra:guest/state-init@1.0.0;",
        )
        .replace("    export tembra:guest/state-init@1.0.0;\n", "");
    let contract = Contract::from_source("reversed.wit", &reversed, None).unwrap();
    let error = contract
        .providers(&BTreeMap::new(), &Grants::default())
        .unwrap_err();
    assert!(error.message.contains("reserved"), "{error}");
    let Err(redefined) = Contract::from_source(
        "guest.wit",
        "package tembra:guest@1.0.0; interface state { read: func() -> string; } world w { import state; }",
        None,
    ) else {
        panic!("an Orb redefined the kernel package");
    };
    assert!(redefined.0.contains("already provided"), "{redefined}");
}

/// The context interface is served by the kernel alone: a world imports it with no export, no
/// provider and no grant, and nothing may export it.
#[test]
fn the_context_interface_is_imported_alone() {
    let aware = r#"
package example:aware@1.0.0;
interface probe { whoami: func() -> string; }
world aware {
    import tembra:guest/context@1.0.0;
    export probe;
}
"#;
    let contract = Contract::from_source("aware.wit", aware, None).unwrap();
    let bindings = contract
        .providers(&BTreeMap::new(), &Grants::default())
        .unwrap();
    assert_eq!(bindings.get(guest::CONTEXT), Some(&Binding::Kernel));
    let plan = contract.plan("aware", 1, &bindings).unwrap();
    assert!(plan.host_imports.is_empty());
    assert!(plan.orb.requires.is_empty());
    assert_eq!(
        plan.orb
            .services
            .iter()
            .map(|service| service.name.as_str())
            .collect::<Vec<_>>(),
        ["aware::example:aware/probe@1.0.0"]
    );
    let exporting = aware.replace(
        "import tembra:guest/context@1.0.0;",
        "export tembra:guest/context@1.0.0;",
    );
    let contract = Contract::from_source("exporting.wit", &exporting, None).unwrap();
    let bindings = contract
        .providers(&BTreeMap::new(), &Grants::default())
        .unwrap();
    let error = contract.plan("aware", 1, &bindings).unwrap_err();
    assert!(error.0.contains("reserved"), "{error}");
}
