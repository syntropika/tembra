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
fn compiled_contribution_metadata_is_typed_bounded_and_verified_on_decode() {
    let contract = Contract::from_source("hooks.wit", "package test:hooks@1.0.0; interface guard { apply: func(value: u32) -> u32; } world extension { import guard; export guard; }", Some("extension")).unwrap();
    let component = component(&contract);
    let contributions = [("test:hooks/guard@1.0.0".into(), -7)].into();
    let roles = Manifest {
        contributions,
        ..Default::default()
    };
    let bytes = contract
        .component_artifact(&component, "extension", 1, &roles)
        .unwrap();
    let decoded = Artifact::decode(&bytes).unwrap();
    assert_eq!(decoded.manifest.contributions, roles.contributions);
    assert!(decoded.manifest.hooks.is_empty());
    let invalid = Manifest {
        contributions: [("test:hooks/missing@1.0.0".into(), 1)].into(),
        ..Default::default()
    };
    assert!(
        contract
            .component_artifact(&component, "extension", 1, &invalid)
            .is_err()
    );
    let identity = Identity {
        name: "extension".into(),
        api_version: 1,
        world: "extension".into(),
    };
    let mut forged = component;
    crate::artifact::append_section(
        &mut forged,
        "tembra:orb",
        &crate::artifact::encode_orb(&identity, contract.prepared(), &invalid),
    );
    assert!(Artifact::decode(&forged).is_err());
    let mut source = crate::artifact::COMPONENT_HEADER.to_vec();
    crate::artifact::append_section(
        &mut source,
        "tembra:orb",
        &crate::artifact::encode_orb(&identity, contract.prepared(), &roles),
    );
    crate::artifact::append_section(&mut source, "tembra:source", b"globalThis.__tembraOrb={}");
    assert!(Artifact::decode(&source).is_err());
}

/// A compiled provider exports the hook it declares and imports it to run its plan. The artifact
/// carries the composition; a shape the composition refuses, a role the world does not export,
/// an interface in both roles and a source bundle claiming the role are all refused.
#[test]
fn compiled_hook_declarations_travel_in_the_artifact_and_are_checked() {
    let contract = Contract::from_source("hooks.wit", "package test:hooks@1.0.0; interface guard { record admission { value: s32, blocked: bool } apply: func(input: admission) -> admission; } interface gate { admit: func(value: s32) -> admission; use guard.{admission}; } world provider { import guard; export guard; export gate; }", Some("provider")).unwrap();
    let component = component(&contract);
    let identity = "test:hooks/guard@1.0.0";
    let roles = Manifest {
        hooks: [(identity.into(), HookComposition::Waterfall)].into(),
        ..Default::default()
    };
    let bytes = contract
        .component_artifact(&component, "gate", 1, &roles)
        .unwrap();
    let decoded = Artifact::decode(&bytes).unwrap();
    assert_eq!(decoded.manifest.hooks, roles.hooks);
    assert!(decoded.manifest.contributions.is_empty());
    for refused in [
        Manifest {
            hooks: [(identity.into(), HookComposition::Series)].into(),
            ..Default::default()
        },
        Manifest {
            hooks: [("test:hooks/gate@1.0.0".into(), HookComposition::Waterfall)].into(),
            ..Default::default()
        },
        Manifest {
            hooks: [(identity.into(), HookComposition::Waterfall)].into(),
            contributions: [(identity.into(), 0)].into(),
            ..Default::default()
        },
    ] {
        assert!(
            contract
                .component_artifact(&component, "gate", 1, &refused)
                .is_err()
        );
    }
    let packaged = Identity {
        name: "gate".into(),
        api_version: 1,
        world: "provider".into(),
    };
    let mut source = crate::artifact::COMPONENT_HEADER.to_vec();
    crate::artifact::append_section(
        &mut source,
        "tembra:orb",
        &crate::artifact::encode_orb(&packaged, contract.prepared(), &roles),
    );
    crate::artifact::append_section(&mut source, "tembra:source", b"globalThis.__tembraOrb={}");
    assert!(Artifact::decode(&source).is_err());
}

/// A compiled service's lifetime and linger travel in the manifest and are applied by the
/// implementation check. A policy for an interface that is no service (a hook callback, a kernel
/// callback, an import, an unknown identity), a lingering host-lifetime object and a source
/// bundle claiming a policy are refused.
#[test]
fn compiled_service_policies_travel_in_the_artifact_and_are_checked() {
    let contract = Contract::from_source("policies.wit", "package test:policies@1.0.0; interface guard { apply: func(value: u32) -> u32; } interface ledger { count: func() -> u32; } world book { import guard; export guard; export ledger; import tembra:guest/state@1.0.0; export tembra:guest/state-init@1.0.0; }", Some("book")).unwrap();
    let component = component(&contract);
    let ledger = "test:policies/ledger@1.0.0";
    let manifest = Manifest {
        hooks: [(
            "test:policies/guard@1.0.0".into(),
            HookComposition::Waterfall,
        )]
        .into(),
        services: [(
            ledger.into(),
            ServicePolicy {
                lifetime: Lifetime::Host,
                linger_ms: 0,
            },
        )]
        .into(),
        ..Default::default()
    };
    let bytes = contract
        .component_artifact(&component, "book", 1, &manifest)
        .unwrap();
    let decoded = Artifact::decode(&bytes).unwrap();
    assert_eq!(decoded.manifest, manifest);
    let lingering = Manifest {
        services: [(
            ledger.into(),
            ServicePolicy {
                lifetime: Lifetime::Referenced,
                linger_ms: 2_500,
            },
        )]
        .into(),
        ..Default::default()
    };
    let decoded = Artifact::decode(
        &contract
            .component_artifact(&component, "book", 1, &lingering)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(decoded.manifest.services[ledger].linger_ms, 2_500);
    let policy = |identity: &str, lifetime, linger_ms| Manifest {
        hooks: manifest.hooks.clone(),
        services: [(
            identity.into(),
            ServicePolicy {
                lifetime,
                linger_ms,
            },
        )]
        .into(),
        ..Default::default()
    };
    for (refused, reason) in [
        (
            policy("test:policies/guard@1.0.0", Lifetime::Host, 0),
            "a hook callback",
        ),
        (
            policy("tembra:guest/state-init@1.0.0", Lifetime::Host, 0),
            "a kernel callback",
        ),
        (
            policy("tembra:guest/state@1.0.0", Lifetime::Host, 0),
            "an import",
        ),
        (
            policy("test:policies/missing@1.0.0", Lifetime::Host, 0),
            "an unknown interface",
        ),
        (
            policy(ledger, Lifetime::Host, 10),
            "a lingering host object",
        ),
    ] {
        assert!(
            contract
                .component_artifact(&component, "book", 1, &refused)
                .is_err(),
            "{reason}"
        );
        let identity = Identity {
            name: "book".into(),
            api_version: 1,
            world: "book".into(),
        };
        let mut forged = component.clone();
        crate::artifact::append_section(
            &mut forged,
            "tembra:orb",
            &crate::artifact::encode_orb(&identity, contract.prepared(), &refused),
        );
        assert!(Artifact::decode(&forged).is_err(), "{reason}");
    }
    let mut source = crate::artifact::COMPONENT_HEADER.to_vec();
    crate::artifact::append_section(
        &mut source,
        "tembra:orb",
        &crate::artifact::encode_orb(
            &Identity {
                name: "book".into(),
                api_version: 1,
                world: "book".into(),
            },
            contract.prepared(),
            &lingering,
        ),
    );
    crate::artifact::append_section(&mut source, "tembra:source", b"globalThis.__tembraOrb={}");
    assert!(Artifact::decode(&source).is_err());
}

#[test]
fn source_and_compiled_artifacts_round_trip_without_executing_source() {
    let contract = contract(
        "package test:artifact; interface values { read: func() -> u32; } world run { export values; }",
    );
    let source = contract
        .source_artifact("throw new Error('must not execute')", "example", 1)
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
        .component_artifact(&compiled, "example", 1, &Default::default())
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
            .component_artifact(&packed, "example", 1, &Default::default())
            .is_err()
    );
}

#[test]
fn malformed_duplicate_unknown_and_forged_artifact_metadata_is_rejected() {
    let contract = contract("package test:artifact; world run {}");
    let original = contract
        .source_artifact("globalThis.__tembraOrb={}", "example", 1)
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
        .component_artifact(&component(&contract), "example", 1, &Default::default())
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
        .component_artifact(&compiled.finish(), "example", 1, &Default::default())
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
                hooks: Default::default(),
                contributes: Default::default(),
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
    let contract = Contract::new(resolve, world, None).unwrap();
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
        &crate::artifact::encode_orb(&identity, claimed.prepared(), &Default::default()),
    );
    let error = Artifact::decode(&bytes).err().unwrap();
    assert!(error.0.contains("export signatures"), "{error}");
    let packed = real
        .component_artifact(&component(&real), "example", 1, &Default::default())
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
    assert!(!interface.resources.is_empty());
    interface.resources.clear();
    let forged = PreparedContract::new(model).unwrap();
    let identity = Identity {
        name: "forged".into(),
        api_version: 1,
        world: "run".into(),
    };
    append(
        &mut bytes,
        "tembra:orb",
        &crate::artifact::encode_orb(&identity, &forged, &Default::default()),
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
fn compiled_resource_identity_cannot_be_relabelled_in_artifact_metadata() {
    let contract = contract(
        "package test:identity; interface handles { resource handle; } interface values { get: func() -> u32; } world run { import handles; export values; }",
    );
    let binary = component(&contract);
    for change_owner in [false, true] {
        let mut model = contract.prepared().model().clone();
        let resource_id = model
            .types
            .iter()
            .position(|kind| matches!(kind, Kind::Resource(_)))
            .unwrap();
        let Kind::Resource(resource) = &mut model.types[resource_id] else {
            unreachable!("selected as a resource")
        };
        if change_owner {
            resource.interface = Some("other:identity/handles".into());
        } else {
            resource.name = Some("counterfeit".into());
        }
        // Claim an alias instead of a direct definition. This remains a structurally valid
        // model, so the binary verifier must reject its false nominal identity independently.
        let alias = model.types.len() as u32;
        model.types.push(Kind::Alias(Ty::Ref(resource_id as u32)));
        let Item::Interface(interface) = &mut model.imports[0].item else {
            panic!("expected the handles interface")
        };
        interface.resources.insert("handle".into(), alias);
        let forged = PreparedContract::new(model).unwrap();
        let mut bytes = binary.clone();
        append(
            &mut bytes,
            "tembra:orb",
            &crate::artifact::encode_orb(
                &Identity {
                    name: "forged".into(),
                    api_version: 1,
                    world: "run".into(),
                },
                &forged,
                &Default::default(),
            ),
        );
        assert!(
            Artifact::decode(&bytes)
                .err()
                .unwrap()
                .0
                .contains("import signatures differ")
        );
    }
}

#[test]
fn compiled_resource_aliases_preserve_the_original_definition() {
    let contract = contract(
        r#"
            package test:aliases;
            interface handles { resource handle; }
            interface aliasing {
                use handles.{handle as renamed};
                inspect: func(value: borrow<renamed>) -> u32;
            }
            interface values { get: func() -> u32; }
            world run { import aliasing; export values; }
        "#,
    );
    let bytes = contract
        .component_artifact(&component(&contract), "aliases", 1, &Default::default())
        .unwrap();
    let artifact = Artifact::decode(&bytes).unwrap();
    let aliasing = artifact
        .contract
        .model()
        .imports
        .iter()
        .find_map(|entry| match &entry.item {
            Item::Interface(interface)
                if interface.contract.as_deref() == Some("test:aliases/aliasing") =>
            {
                Some(interface)
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(
        artifact
            .contract
            .resource_identity(Ty::Ref(aliasing.resources["renamed"])),
        Some(&ResourceIdentity {
            interface: Some("test:aliases/handles".into()),
            name: Some("handle".into()),
        })
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
        .component_artifact(&component(&retained), "retained", 1, &Default::default())
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
        .component_artifact(&component(&declared), "resources", 1, &Default::default())
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
        .component_artifact(&component(&retained), "pruned", 1, &Default::default())
        .unwrap();
    Artifact::decode(&bytes).unwrap();
}

#[test]
fn compiled_resource_exports_are_verified_but_need_a_capability_transport() {
    let declared = contract(
        "package test:unsupported; interface handles { resource handle { read: func() -> u32; } } world run { export handles; }",
    );
    let bytes = declared
        .component_artifact(&component(&declared), "sessions", 1, &Default::default())
        .unwrap();
    let artifact = Artifact::decode(&bytes).unwrap();
    artifact.contract.check_resource_profile().unwrap();
    assert!(artifact.contract.check_value_profile().is_err());
    assert!(
        declared
            .source_artifact("export default {}", "sessions", 1)
            .is_err()
    );

    let nested = contract(
        "package test:unsupported; interface handles { resource handle; \
         open: func() -> option<own<handle>>; } world run { export handles; }",
    );
    assert!(
        nested
            .component_artifact(&component(&nested), "nested", 1, &Default::default())
            .is_err()
    );
}
