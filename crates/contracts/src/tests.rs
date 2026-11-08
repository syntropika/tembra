use super::*;
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
            world: "provider".into(),
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
    let mut bindings = bindings();
    let Binding::Orb { world, .. } = bindings.values_mut().next().unwrap() else {
        unreachable!()
    };
    *world = "consumer".into();
    assert!(
        contract
            .plan("consumer", 1, &bindings)
            .unwrap_err()
            .0
            .contains("does not export")
    );
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
    assert!(output.plan("output", 1, &BTreeMap::new()).is_err());
}

#[test]
fn rich_types_and_async_signatures_survive_inspection() {
    let source = "package example:rich; interface values { resource file; record value { body: stream<u8>, done: future<u32> } use-file: async func(f: borrow<file>) -> own<file>; } world test { export values; }";
    let contract = Contract::from_source("rich.wit", source, None).unwrap();
    let description = contract.describe().to_string();
    for expected in ["stream", "future", "borrow", "own", "resource", "async"] {
        assert!(description.contains(expected), "missing {expected}");
    }
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
    let interface = "example:calculation/arithmetic@1.2.0";
    assert!(
        contract
            .validate_arguments(interface, "add", &serde_json::json!([{"left":1,"right":2}]))
            .is_ok()
    );
    assert!(
        contract
            .validate_arguments(
                interface,
                "add",
                &serde_json::json!([{"left":2147483648_u64,"right":2}])
            )
            .is_err()
    );
    assert!(
        contract
            .validate_arguments(
                interface,
                "add",
                &serde_json::json!([{"left":1,"right":2,"extra":3}])
            )
            .is_err()
    );
    assert!(
        contract
            .validate_arguments(interface, "add", &serde_json::json!([]))
            .is_err()
    );
    assert!(
        contract
            .validate_result(interface, "add", &serde_json::json!(3))
            .is_ok()
    );
    assert!(
        contract
            .validate_result(interface, "add", &serde_json::json!("3"))
            .is_err()
    );
    assert!(contract.check_value_profile().is_err());
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
    assert!(
        float
            .validate_result("example:values/values", "read", &serde_json::json!(-0.0))
            .is_err()
    );
    let consumer = Contract::from_source("example.wit", SOURCE, Some("consumer")).unwrap();
    assert!(
        consumer
            .check_import_value_profile("example:calculation/arithmetic@1.2.0")
            .is_err()
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
