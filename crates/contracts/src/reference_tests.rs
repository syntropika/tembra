//! Checked keyed-reference roles: shape checks, derived provider requirements, artifact metadata
//! and browser prepared-component parity.
use crate::{
    Artifact, Binding, ComponentRoles, Contract, FunctionKind, Item, PreparedComponent,
    PreparedContract, ResourceIdentity,
    artifact::{COMPONENT_HEADER, Identity, append_section, encode_orb_roles},
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

const VIEW: &str = "example:keyed/counter-ref@1.0.0";
const SERVICE: &str = "example:keyed/counter@1.0.0";
const QUALIFIED: &str = "keyed::example:keyed/counter@1.0.0";

/// The provider service and its keyed view, with `view` as the body of `counter-ref`.
fn wit(service: &str, view: &str) -> String {
    format!(
        "package example:keyed@1.0.0;
         interface counter {{ {service} }}
         interface counter-ref {{ {view} }}
         interface router {{ route: func(a: string, b: string, amount: u32) -> string; }}
         world provider {{ export counter; }}
         world compiled-router {{ import counter-ref; export router; }}
         world mixed {{ import counter; import counter-ref; export router; }}"
    )
}

const SERVICE_BODY: &str = "add: func(amount: u32) -> u32; who: func() -> string;";
const VIEW_BODY: &str = "resource counter { add: func(amount: u32) -> u32; who: func() -> string; }
     get: async func(key: list<string>) -> result<counter, string>;";

fn contract(source: &str, world: &str) -> Contract {
    Contract::from_source("keyed.wit", source, Some(world)).unwrap()
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

fn roles(binding: &str, target: &str) -> ComponentRoles {
    ComponentRoles {
        references: [(binding.to_owned(), target.to_owned())].into(),
        ..Default::default()
    }
}

/// The interface digest a separately prepared provider publishes for its `counter` service.
fn provider_signature(service: &str) -> String {
    let provider = contract(&wit(service, VIEW_BODY), "provider");
    let plan = provider.plan("keyed", 1, &BTreeMap::new()).unwrap();
    plan.orb.services[0].signature.clone().unwrap()
}

fn orb_binding() -> BTreeMap<String, Binding> {
    [(
        VIEW.to_owned(),
        Binding::Orb {
            name: "keyed".into(),
            api_version: 1,
        },
    )]
    .into()
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

fn ok(response: String) -> Value {
    let response: Value = serde_json::from_str(&response).unwrap();
    assert!(response.get("ok").is_some(), "{response}");
    response["ok"].clone()
}

/// Package a role without the packaging check, as a forged artifact would.
fn forge(contract: &Contract, roles: &ComponentRoles, source: bool) -> Vec<u8> {
    let identity = Identity {
        name: "router".into(),
        api_version: 1,
        world: contract.prepared().world().to_owned(),
    };
    let mut bytes = if source {
        COMPONENT_HEADER.to_vec()
    } else {
        component(contract)
    };
    append_section(
        &mut bytes,
        "tembra:orb",
        &encode_orb_roles(&identity, contract.prepared(), roles),
    );
    if source {
        append_section(&mut bytes, "tembra:source", b"globalThis.__tembraOrb={}");
    }
    bytes
}

#[test]
fn reference_roles_derive_the_provider_requirement_and_round_trip() {
    let importer = contract(&wit(SERVICE_BODY, VIEW_BODY), "compiled-router");
    let bytes = importer
        .component_artifact(&component(&importer), "router", 1, &roles(VIEW, SERVICE))
        .unwrap();
    let artifact = Artifact::decode(&bytes).unwrap();
    let contract = &artifact.contract;
    let signature = provider_signature(SERVICE_BODY);

    let reference = &contract.references()[VIEW];
    assert_eq!(reference.interface, SERVICE);
    assert_eq!(
        reference.resource,
        ResourceIdentity {
            interface: Some(VIEW.into()),
            name: Some("counter".into()),
        }
    );
    assert_eq!(reference.acquire, "get");
    assert_eq!(
        reference.methods,
        BTreeMap::from([
            ("[method]counter.add".to_owned(), "add".to_owned()),
            ("[method]counter.who".to_owned(), "who".to_owned()),
        ])
    );
    assert_eq!(reference.signature, signature);

    // The requirement targets the provider service with its exact digest and method names.
    let plan = contract.plan("router", 1, &orb_binding()).unwrap();
    let requirement = &plan.orb.requires[0];
    assert_eq!(requirement.orb, "keyed");
    assert_eq!(requirement.signatures[QUALIFIED], signature);
    assert_eq!(
        requirement.methods[QUALIFIED],
        BTreeSet::from(["add".to_owned(), "who".to_owned()])
    );
    assert_eq!(
        contract.import_services(&orb_binding()).unwrap(),
        BTreeMap::from([(VIEW.to_owned(), QUALIFIED.to_owned())])
    );
    // A view never binds to a host provider.
    let host = [(
        VIEW.to_owned(),
        Binding::Host {
            provider: "anything".into(),
        },
    )]
    .into();
    assert!(contract.plan("router", 1, &host).is_err());

    // The original WIT stays intact: the view import keeps its resource, methods and kinds.
    let Item::Interface(view) = &contract.model().imports[0].item else {
        panic!("view import")
    };
    assert_eq!(view.contract.as_deref(), Some(VIEW));
    assert!(view.resources.contains_key("counter"));
    assert!(
        view.functions
            .iter()
            .any(|function| function.name == "get"
                && function.kind == FunctionKind::AsyncFreestanding)
    );

    // Every data profile admits the checked view; the role metadata is described as checked.
    for check in [
        PreparedContract::check_import_value_profile,
        PreparedContract::check_import_source_profile,
        PreparedContract::check_import_resource_profile,
    ] {
        check(contract, VIEW).unwrap();
    }
    let described = contract.describe();
    assert_eq!(
        described["references"],
        json!({VIEW: {
            "interface": SERVICE,
            "resource": {"interface": VIEW, "name": "counter"},
            "acquire": "get",
            "methods": {"[method]counter.add": "add", "[method]counter.who": "who"},
            "signature": signature,
        }})
    );
    let install = ok(contract.request(&format!(
        r#"{{"kind":"install","name":"router","apiVersion":1,"requires":{{"{VIEW}":{{"orb":"keyed","apiVersion":1}}}}}}"#
    )));
    assert_eq!(install["importServices"][VIEW], QUALIFIED);
    assert_eq!(
        install["plan"]["orb"]["requires"][0]["signatures"][QUALIFIED],
        signature
    );

    // Browser metadata: the prepared component decodes the same checked roles.
    let prepared = PreparedComponent::decode(&bytes, &[sha256(&bytes)]).unwrap();
    assert!(prepared.contract == artifact.contract);
    assert_eq!(prepared.contract.references(), contract.references());
    assert_eq!(prepared.contract.describe(), described);
    let install_packet = format!(
        r#"{{"kind":"install","name":"router","apiVersion":1,"requires":{{"{VIEW}":{{"orb":"keyed","apiVersion":1}}}}}}"#
    );
    assert_eq!(
        prepared.contract.request(&install_packet),
        contract.request(&install_packet)
    );

    // Re-packaging the decoded contract's metadata reproduces the same bytes.
    assert_eq!(
        importer
            .component_artifact(&component(&importer), "router", 1, &roles(VIEW, SERVICE))
            .unwrap(),
        bytes
    );
}

#[test]
fn view_method_slots_carry_only_data_while_resource_wire_keeps_the_receiver() {
    let importer = contract(&wit(SERVICE_BODY, VIEW_BODY), "compiled-router");
    let bytes = importer
        .component_artifact(&component(&importer), "router", 1, &roles(VIEW, SERVICE))
        .unwrap();
    let prepared = PreparedComponent::decode(&bytes, &[sha256(&bytes)]).unwrap();
    let artifact = Artifact::decode(&bytes).unwrap();
    for contract in [&prepared.contract, &artifact.contract] {
        let add = contract.bind(VIEW, "[method]counter.add", false).unwrap();
        contract.validate_slot(add, true, &json!([5])).unwrap();
        assert!(contract.validate_slot(add, true, &json!([0, 5])).is_err());
        contract.validate_slot(add, false, &json!(7)).unwrap();
        assert_eq!(contract.to_wire(add, true, json!([5])).unwrap(), json!([5]));
        assert_eq!(
            contract.from_wire(add, true, json!([5])).unwrap(),
            json!([5])
        );
        assert_eq!(contract.validate_packet(add, true, "[5]"), r#"{"ok":null}"#);
        assert!(
            contract
                .validate_packet(add, true, "[0,5]")
                .contains("error")
        );
        let who = contract.bind(VIEW, "[method]counter.who", false).unwrap();
        contract.validate_slot(who, true, &json!([])).unwrap();
        // The resource wire still reads the original method and strips its implicit receiver.
        let signature = contract.resource_signature(add).unwrap();
        assert_eq!(
            signature
                .receiver
                .and_then(|resource| resource.name.as_deref()),
            Some("counter")
        );
        assert_eq!(
            contract.from_resource_wire(add, true, json!([5])).unwrap(),
            json!([5])
        );
    }
    // Without a role the same interface is an ordinary guest-resource import whose slots keep
    // every parameter and which the data profiles refuse.
    let plain = importer
        .component_artifact(&component(&importer), "router", 1, &Default::default())
        .unwrap();
    let plain = Artifact::decode(&plain).unwrap().contract;
    assert!(plain.references().is_empty());
    assert_eq!(plain.describe()["references"], json!({}));
    let add = plain.bind(VIEW, "[method]counter.add", false).unwrap();
    assert!(plain.validate_slot(add, true, &json!([5])).is_err());
    assert!(plain.check_import_value_profile(VIEW).is_err());
    assert!(plain.check_import_resource_profile(VIEW).is_err());
    assert_eq!(
        plain.import_services(&orb_binding()).unwrap()[VIEW],
        format!("keyed::{VIEW}")
    );
}

/// The digest includes the function kind, so a view method is `async` exactly when the
/// provider function is. No sync/async normalization is applied.
#[test]
fn view_method_kinds_must_match_the_provider_declaration() {
    let async_service = "add: async func(amount: u32) -> u32; who: func() -> string;";
    let async_view =
        "resource counter { add: async func(amount: u32) -> u32; who: func() -> string; }
         get: async func(key: list<string>) -> result<counter, string>;";
    let derived = |view: &str| {
        let importer = contract(&wit(SERVICE_BODY, view), "compiled-router");
        importer
            .prepared()
            .reference_bindings(&roles(VIEW, SERVICE).references)
            .unwrap()[VIEW]
            .signature
            .clone()
    };
    assert_eq!(derived(async_view), provider_signature(async_service));
    assert_ne!(derived(async_view), provider_signature(SERVICE_BODY));
    assert_ne!(derived(VIEW_BODY), provider_signature(async_service));
    // Parameter names are part of the provider digest as well.
    let renamed = "resource counter { add: func(value: u32) -> u32; who: func() -> string; }
         get: async func(key: list<string>) -> result<counter, string>;";
    assert_ne!(derived(renamed), provider_signature(SERVICE_BODY));

    // A plain import and a view of the same service must agree on its signature.
    let mixed = |view: &str| {
        let importer = contract(&wit(SERVICE_BODY, view), "mixed");
        let mut bindings = orb_binding();
        bindings.insert(
            SERVICE.into(),
            Binding::Orb {
                name: "keyed".into(),
                api_version: 1,
            },
        );
        importer
            .prepared()
            .clone()
            .with_references(&roles(VIEW, SERVICE).references)
            .unwrap()
            .plan("router", 1, &bindings)
    };
    let plan = mixed(VIEW_BODY).unwrap();
    assert_eq!(plan.orb.requires.len(), 1);
    assert_eq!(
        plan.orb.requires[0].signatures[QUALIFIED],
        provider_signature(SERVICE_BODY)
    );
    assert!(mixed(async_view).is_err());
}

#[test]
fn malformed_reference_views_are_refused_when_packaged_and_decoded() {
    let get = "get: async func(key: list<string>) -> result<counter, string>;";
    let methods = "add: func(amount: u32) -> u32;";
    let refused = [
        format!("resource counter {{ constructor(); {methods} }} {get}"),
        format!("resource counter {{ make: static func() -> counter; {methods} }} {get}"),
        format!("resource counter {{ {methods} }} {get} peek: func() -> u32;"),
        format!("resource counter {{ {methods} }}"),
        format!(
            "resource counter {{ {methods} }} get: func(key: list<string>) -> result<counter, string>;"
        ),
        format!(
            "resource counter {{ {methods} }} get: async func(key: string) -> result<counter, string>;"
        ),
        format!(
            "resource counter {{ {methods} }} get: async func(key: list<u8>) -> result<counter, string>;"
        ),
        format!(
            "resource counter {{ {methods} }} get: async func(key: list<string>, other: u32) -> result<counter, string>;"
        ),
        format!("resource counter {{ {methods} }} get: async func(key: list<string>) -> counter;"),
        format!(
            "resource counter {{ {methods} }} get: async func(key: list<string>) -> result<counter, u32>;"
        ),
        format!(
            "resource counter {{ {methods} }} get: async func(key: list<string>) -> result<counter>;"
        ),
        format!(
            "resource counter {{ {methods} }} get: async func(key: list<string>) -> option<counter>;"
        ),
        format!("resource counter {{ {methods} }} resource other; {get}"),
        format!("resource counter; {get}"),
        format!("resource counter {{ {methods} link: func(other: borrow<counter>); }} {get}"),
        format!("resource counter {{ {methods} fork: func() -> counter; }} {get}"),
        format!("resource counter {{ {methods} frames: async func() -> stream<u8>; }} {get}"),
        format!("resource counter {{ {methods} wide: func(value: option<option<u32>>); }} {get}"),
    ];
    for view in &refused {
        let importer = contract(&wit(SERVICE_BODY, view), "compiled-router");
        let error = importer
            .component_artifact(&component(&importer), "router", 1, &roles(VIEW, SERVICE))
            .unwrap_err();
        assert!(error.0.starts_with("reference view "), "{view}: {error}");
        // A forged artifact that skipped the packaging check is refused on every decode path.
        let forged = forge(&importer, &roles(VIEW, SERVICE), false);
        assert!(Artifact::decode(&forged).is_err(), "{view}");
        assert!(
            PreparedComponent::decode(&forged, &[sha256(&forged)]).is_err(),
            "{view}"
        );
    }

    // A resource used from another interface is not the view's own resource.
    let used = "package example:keyed@1.0.0;
        interface counter { add: func(amount: u32) -> u32; }
        interface base { resource counter { add: func(amount: u32) -> u32; } }
        interface counter-ref { use base.{counter}; get: async func(key: list<string>) -> result<counter, string>; }
        world compiled-router { import counter-ref; }";
    let importer = contract(used, "compiled-router");
    assert!(
        importer
            .component_artifact(&component(&importer), "router", 1, &roles(VIEW, SERVICE))
            .is_err()
    );

    // Malformed roles: unknown binding, invalid or self-addressed target, and a plain import.
    let importer = contract(&wit(SERVICE_BODY, VIEW_BODY), "mixed");
    for (binding, target) in [
        ("example:keyed/missing@1.0.0", SERVICE),
        (VIEW, "not an identity"),
        (VIEW, VIEW),
        (SERVICE, "example:keyed/other@1.0.0"),
        ("example:keyed/router@1.0.0", SERVICE),
    ] {
        assert!(
            importer
                .component_artifact(&component(&importer), "router", 1, &roles(binding, target))
                .is_err(),
            "{binding} -> {target}"
        );
    }

    // Getters, setters and other scoped kinds cannot be spelled by this WIT parser, so the
    // check is exercised on the prepared model directly.
    let importer = contract(&wit(SERVICE_BODY, VIEW_BODY), "compiled-router");
    let mut model = importer.prepared().model().clone();
    let Item::Interface(view) = &mut model.imports[0].item else {
        panic!("view import")
    };
    let resource = *view.resources.values().next().unwrap();
    let method = view
        .functions
        .iter()
        .position(|function| function.name == "[method]counter.who")
        .unwrap();
    for kind in [
        FunctionKind::MethodGetter(resource),
        FunctionKind::MethodSetter(resource),
        FunctionKind::Static(resource),
        FunctionKind::AsyncStatic(resource),
        FunctionKind::StaticGetter(resource),
        FunctionKind::Getter,
    ] {
        let mut model = model.clone();
        let Item::Interface(view) = &mut model.imports[0].item else {
            unreachable!()
        };
        view.functions[method].kind = kind;
        assert!(
            PreparedContract::new(model)
                .unwrap()
                .reference_bindings(&roles(VIEW, SERVICE).references)
                .is_err(),
            "{kind:?}"
        );
    }
    // A method whose spelling does not name the view resource is refused.
    let mut renamed = model.clone();
    let Item::Interface(view) = &mut renamed.imports[0].item else {
        unreachable!()
    };
    view.functions[method].name = "[method]other.who".into();
    let prepared = PreparedContract::new(renamed).unwrap();
    assert!(
        prepared
            .reference_bindings(&roles(VIEW, SERVICE).references)
            .is_err()
    );
}

#[test]
fn source_artifacts_and_duplicate_roles_are_refused() {
    // A source world cannot import a view at all: its data profile admits no resources. Even a
    // forged role on a plain source contract is refused on decode.
    let source = contract(
        "package example:keyed@1.0.0;
         interface counter { add: func(amount: u32) -> u32; }
         interface router { route: func() -> u32; }
         world source { import counter; export router; }",
        "source",
    );
    let forged = forge(&source, &roles(SERVICE, "example:keyed/other@1.0.0"), true);
    let error = Artifact::decode(&forged).err().unwrap();
    assert!(error.0.contains("reference view roles"), "{error}");
    // Source artifacts encode empty roles and still decode.
    let packaged = source
        .source_artifact("globalThis.__tembraOrb={}", "router", 1)
        .unwrap();
    assert!(
        Artifact::decode(&packaged)
            .unwrap()
            .contract
            .references()
            .is_empty()
    );

    // Duplicate role bindings in metadata are refused.
    let importer = contract(&wit(SERVICE_BODY, VIEW_BODY), "compiled-router");
    let mut payload = encode_orb_roles(
        &Identity {
            name: "router".into(),
            api_version: 1,
            world: "compiled-router".into(),
        },
        importer.prepared(),
        &roles(VIEW, SERVICE),
    );
    // Replace the trailing one-entry reference map with two identical entries.
    let mut entry = crate::codec::Writer::default();
    entry.str(VIEW);
    entry.str(SERVICE);
    let entry = entry.finish();
    let start = payload.len() - entry.len() - 1;
    assert_eq!(payload[start], 1);
    payload.truncate(start);
    payload.push(2);
    payload.extend_from_slice(&entry);
    payload.extend_from_slice(&entry);
    let mut bytes = component(&importer);
    append_section(&mut bytes, "tembra:orb", &payload);
    let error = Artifact::decode(&bytes).err().unwrap();
    assert!(error.0.contains("duplicate reference view role"), "{error}");
}
