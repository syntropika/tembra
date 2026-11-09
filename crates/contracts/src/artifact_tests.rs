use super::*;
use std::borrow::Cow;
use std::collections::BTreeMap;
use wasm_encoder::{CustomSection, Encode, Section};

fn contract(source: &str) -> Contract {
    Contract::from_source("artifact.wit", source, None).unwrap()
}

fn component(contract: &Contract) -> Vec<u8> {
    let mut module = wit_component::dummy_module(
        &contract.resolve,
        contract.world,
        wit_parser::ManglingAndAbi::Standard32,
    );
    wit_component::embed_component_metadata(
        &mut module,
        &contract.resolve,
        contract.world,
        wit_component::StringEncoding::UTF8,
        false,
    )
    .unwrap();
    wit_component::ComponentEncoder::default()
        .module(&module)
        .unwrap()
        .validate(true)
        .encode()
        .unwrap()
}

fn append(bytes: &mut Vec<u8>, name: &str, data: &[u8]) {
    let section = CustomSection {
        name: Cow::Borrowed(name),
        data: Cow::Borrowed(data),
    };
    bytes.push(section.id());
    section.encode(bytes);
}

#[test]
fn source_and_compiled_artifacts_round_trip_without_executing_source() {
    let contract = contract(
        "package test:artifact; interface values { read: func() -> u32; } world run { export values; }",
    );
    let source = contract
        .artifact(
            Some("throw new Error('must not execute')"),
            None,
            "example",
            1,
        )
        .unwrap();
    let decoded = Artifact::decode(&source).unwrap();
    assert_eq!(
        match &decoded.implementation {
            ArtifactImplementation::Source(source) => Some(source.as_str()),
            _ => None,
        },
        Some("throw new Error('must not execute')")
    );
    assert_eq!(decoded.identity.name, "example");
    let compiled = component(&contract);
    let packed = contract
        .artifact(None, Some(&compiled), "example", 1)
        .unwrap();
    let decoded = Artifact::decode(&packed).unwrap();
    assert_eq!(
        match decoded.implementation {
            ArtifactImplementation::Component(bytes) => bytes,
            _ => panic!("expected component"),
        },
        packed
    );

    assert!(
        contract
            .artifact(None, Some(&packed), "example", 1)
            .is_err()
    );
}

#[test]
fn malformed_duplicate_unknown_and_forged_artifact_metadata_is_rejected() {
    let contract = contract("package test:artifact; world run {}");
    let original = contract
        .artifact(Some("globalThis.__tembraOrb={}"), None, "example", 1)
        .unwrap();
    for (name, data) in [
        ("tembra:source", b"other".as_slice()),
        ("tembra:unknown", b"value"),
        ("tembra:orb", br#"{"name":" ","apiVersion":0}"#),
    ] {
        let mut bytes = original.clone();
        append(&mut bytes, name, data);
        assert!(Artifact::decode(&bytes).is_err(), "accepted {name}");
    }
    assert!(Artifact::decode(&contract.encode().unwrap()).is_err());
    let mut compiled = contract
        .artifact(None, Some(&component(&contract)), "example", 1)
        .unwrap();
    append(&mut compiled, "tembra:source", b"pretend source");
    assert!(Artifact::decode(&compiled).is_err());
}

#[test]
fn nested_component_metadata_cannot_override_the_containing_orb() {
    let contract = contract("package test:artifact; world run {}");
    let mut nested = wasm_encoder::Module::new();
    nested.section(&CustomSection {
        name: Cow::Borrowed("tembra:source"),
        data: Cow::Borrowed(b"ignored"),
    });
    let mut compiled = wasm_encoder::Component::new();
    compiled.section(&wasm_encoder::ModuleSection(&nested));
    let packed = contract
        .artifact(None, Some(&compiled.finish()), "example", 1)
        .unwrap();
    assert!(matches!(
        Artifact::decode(&packed).unwrap().implementation,
        ArtifactImplementation::Component(_)
    ));
}

#[test]
fn compiled_imports_must_match_the_declared_world_before_linking() {
    let expected = contract(
        "package test:artifact; interface values { read: func() -> u32; } world run { export values; }",
    );
    let hidden = contract(
        "package test:artifact; interface values { read: func() -> u32; } interface secret { read: func() -> u32; } world run { import secret; export values; }",
    );
    assert!(
        expected
            .check_component_exports(&component(&hidden))
            .unwrap_err()
            .0
            .contains("import signatures")
    );
}

#[test]
fn canonical_javascript_names_resolve_imports_without_reverse_guessing() {
    let contract = contract(
        "package test:names; interface values { get-HTTP: func() -> u32; } world run { import values; }",
    );
    assert_eq!(javascript_name("get-HTTP"), "getHttp");
    assert_eq!(
        contract.describe()["javascriptNames"]["get-HTTP"],
        "getHttp"
    );
    // Guests map spellings through `javascriptNames`; slots bind only the original WIT name.
    assert!(
        contract
            .bind("test:names/values", "get-HTTP", false)
            .is_ok()
    );
    for spelling in ["getHttp", "getHTTP"] {
        assert!(contract.bind("test:names/values", spelling, false).is_err());
    }
}

#[test]
fn checked_implementations_share_aliases_lifetimes_and_original_method_names() {
    let contract = contract(
        "package test:names; interface values { get-HTTP: func() -> u32; } world run { export values; }",
    );
    let services = contract
        .check_implementation(
            "example",
            1,
            Implementation {
                name: Some("example".into()),
                api_version: Some(1),
                services: BTreeMap::from([(
                    "values".into(),
                    ServiceImplementation {
                        methods: std::collections::BTreeSet::from(["getHttp".into()]),
                        lifetime: Some(Lifetime::Host),
                        linger: Some(17),
                        access: false,
                    },
                )]),
            },
        )
        .unwrap();
    assert_eq!(services.len(), 1);
    let service = &services[0];
    assert_eq!(service.name, "example::test:names/values");
    assert_eq!(service.interface, "test:names/values");
    assert_eq!(service.alias, "values");
    assert_eq!(service.methods["get-HTTP"], "getHttp");
    assert_eq!(
        tembra_core::Lifetime::from(service.lifetime),
        tembra_core::Lifetime::Host
    );
    assert_eq!(service.linger, 17);
    assert!(service.signature.is_some());
}

#[test]
fn colliding_javascript_service_and_method_names_fail_before_installation() {
    let mut resolve = wit_parser::Resolve::default();
    resolve
        .push_str(
            "one.wit",
            "package test:one; interface store { read: func(); }",
        )
        .unwrap();
    resolve
        .push_str(
            "two.wit",
            "package test:two; interface store { read: func(); }",
        )
        .unwrap();
    let package = resolve
        .push_str(
            "world.wit",
            "package test:composition; world run { export test:one/store; export test:two/store; }",
        )
        .unwrap();
    let world = resolve.select_world(&[package], None).unwrap();
    let contract = Contract::new(resolve, world).unwrap();
    let error = contract.plan("stores", 1, &BTreeMap::new()).unwrap_err();
    assert!(error.0.contains("ambiguous JavaScript service store"));
    assert!(Contract::from_source("methods.wit", "package test:names; interface values { get-HTTP: func(); get-http: func(); } world run { export values; }", None).is_err());
}

#[test]
fn forged_compiled_contracts_are_checked_against_the_real_component() {
    let real = contract(
        "package test:artifact; interface values { read: func() -> u32; } world run { export values; }",
    );
    let claimed = contract(
        "package test:artifact; interface values { read: func() -> string; } world run { export values; }",
    );
    let identity = Identity {
        name: "example".into(),
        api_version: 1,
        world: "run".into(),
    };
    let mut bytes = component(&real);
    crate::artifact::append_section(
        &mut bytes,
        "tembra:orb",
        &crate::artifact::encode_orb(&identity, claimed.prepared()),
    );
    let error = Artifact::decode(&bytes).err().unwrap();
    assert!(error.0.contains("export signatures"), "{error}");
    let packed = real
        .artifact(None, Some(&component(&real)), "example", 1)
        .unwrap();
    let decoded = Artifact::decode(&packed).unwrap();
    assert_eq!(decoded.contract, *real.prepared());
    assert_eq!(decoded.contract.describe(), real.describe());
}

#[test]
fn compiled_resource_only_imports_cannot_hide_their_provider_requirement() {
    let contract = contract(
        "package test:handle-checks; interface handles { resource handle; } interface values { get: func() -> u32; } world run { import handles; export values; }",
    );
    let mut bytes = component(&contract);
    contract.check_component_exports(&bytes).unwrap();
    let mut model = contract.prepared().model().clone();
    let Item::Interface(interface) = &mut model.imports[0].item else {
        panic!("expected an interface");
    };
    assert!(interface.resources);
    interface.resources = false;
    let forged = PreparedContract::new(model).unwrap();
    let identity = Identity {
        name: "forged".into(),
        api_version: 1,
        world: "run".into(),
    };
    append(
        &mut bytes,
        "tembra:orb",
        &crate::artifact::encode_orb(&identity, &forged),
    );
    assert!(
        Artifact::decode(&bytes)
            .err()
            .unwrap()
            .0
            .contains("import signatures differ")
    );
}

#[test]
fn compiled_imports_may_eliminate_unused_methods_but_cannot_change_retained_signatures() {
    let make = |methods: &str| {
        contract(&format!(
            "package test:retained; interface storage {{ {methods} }} interface values {{ read: func() -> u32; }} world run {{ import storage; export values; }}"
        ))
    };
    let declared =
        make("read: func(path: string) -> string; write: func(path: string, value: string);");
    let retained = make("read: func(path: string) -> string;");
    let artifact = declared
        .artifact(None, Some(&component(&retained)), "retained", 1)
        .unwrap();
    Artifact::decode(&artifact).unwrap();
    for changed in [
        "read: func(path: u32) -> string;",
        "read: func(path: string) -> u32;",
        "read: func(path: string) -> string; remove: func(path: string);",
    ] {
        assert!(
            declared
                .check_component_exports(&component(&make(changed)))
                .is_err()
        );
    }
}

#[test]
fn compiled_resource_imports_keep_handle_identity_and_function_kinds() {
    let make = |result: &str| {
        contract(&format!(
            "package test:resources; interface storage {{ resource folder; resource file {{ parent: func() -> {result}; }} }} interface values {{ read: func() -> u32; }} world run {{ import storage; export values; }}"
        ))
    };
    let declared = make("own<folder>");
    let artifact = declared
        .artifact(None, Some(&component(&declared)), "resources", 1)
        .unwrap();
    Artifact::decode(&artifact).unwrap();
    assert!(
        declared
            .check_component_exports(&component(&make("own<file>")))
            .is_err()
    );
}

#[test]
fn compiled_imports_may_eliminate_unused_resource_types() {
    let declared = contract(
        "package test:pruned; interface storage { resource unused; read: func() -> u32; } interface values { read: func() -> u32; } world run { import storage; export values; }",
    );
    let retained = contract(
        "package test:pruned; interface storage { read: func() -> u32; } interface values { read: func() -> u32; } world run { import storage; export values; }",
    );
    let bytes = declared
        .artifact(None, Some(&component(&retained)), "pruned", 1)
        .unwrap();
    Artifact::decode(&bytes).unwrap();
}

#[test]
fn compiled_resource_exports_are_refused_during_authoring() {
    let declared = contract(
        "package test:unsupported; interface handles { resource handle { read: func() -> u32; } } world run { export handles; }",
    );
    assert!(
        declared
            .artifact(None, Some(&component(&declared)), "unsupported", 1)
            .is_err()
    );
}
