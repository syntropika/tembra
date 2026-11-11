//! Checked resource positions and exact64 data without capability authority.
use super::*;
use crate::Contract;

fn contract(body: &str) -> Contract {
    Contract::from_source(
        "resource.wit",
        &format!(
            "package test:resources@1.0.0; interface sessions {{ {body} }} \
             world app {{ import sessions; export sessions; }}"
        ),
        Some("app"),
    )
    .unwrap()
}

#[test]
fn constructors_methods_statics_and_owned_factories_have_distinct_roles() {
    let contract = contract(
        "resource session { constructor(seed: u64); read: func() -> u64; \
         version: static func() -> string; } create: func(seed: u64) -> own<session>;",
    );
    contract.check_resource_profile().unwrap();
    contract
        .check_import_resource_profile("test:resources/sessions@1.0.0")
        .unwrap();
    let identity = ResourceIdentity {
        interface: Some("test:resources/sessions@1.0.0".into()),
        name: Some("session".into()),
    };
    for (name, receiver, result) in [
        ("[constructor]session", false, true),
        ("[method]session.read", true, false),
        ("[static]session.version", false, false),
        ("create", false, true),
    ] {
        let slot = contract
            .bind("test:resources/sessions@1.0.0", name, true)
            .unwrap();
        let signature = contract.resource_signature(slot).unwrap();
        assert_eq!(signature.receiver, receiver.then_some(&identity), "{name}");
        assert_eq!(signature.owned_results, result, "{name}");
    }
    assert!(contract.check_value_profile().is_err());
    assert!(
        contract
            .check_import_value_profile("test:resources/sessions@1.0.0")
            .is_err()
    );
    assert!(contract.resource_signature(u32::MAX).is_err());
    // The JSON implementation request is independently guarded; bypassing Install does not
    // register a resource service on a port without capability attachments.
    let response = contract.request(
        &serde_json::json!({
            "kind": "implementation", "name": "sessions", "apiVersion": 1,
            "value": { "services": {} }
        })
        .to_string(),
    );
    let response: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert_eq!(response["error"]["reason"], "wit_value_profile");
}

#[test]
fn unsupported_transfers_and_async_containers_stay_closed() {
    for body in [
        "resource session; take: func(value: own<session>);",
        "resource session; read: func(value: borrow<session>);",
        "read: func() -> stream<u8>;",
        "read: func() -> future<u64>;",
    ] {
        let contract = contract(body);
        assert!(contract.check_resource_profile().is_err(), "{body}");
        assert!(
            contract
                .check_import_resource_profile("test:resources/sessions@1.0.0")
                .is_err(),
            "{body}"
        );
    }
}

#[test]
fn resource_receivers_and_constructor_results_cannot_change_type() {
    let contract = contract("resource session { constructor(); read: func(); } resource other;");
    let mutate = |name: &str, change: fn(&mut Function, u32)| {
        let mut model = contract.model().clone();
        let Item::Interface(interface) = &mut model.exports[0].item else {
            unreachable!()
        };
        let other = interface.resources["other"];
        change(
            interface
                .functions
                .iter_mut()
                .find(|function| function.name == name)
                .unwrap(),
            other,
        );
        let model = PreparedContract::new(model).unwrap();
        assert!(model.check_resource_profile().is_err(), "{name}");
    };
    mutate("[constructor]session", |function, _| function.result = None);
    mutate("[constructor]session", |function, other| {
        function.kind = FunctionKind::Constructor(other)
    });
    mutate("[method]session.read", |function, _| {
        function.params.clear()
    });
    mutate("[method]session.read", |function, other| {
        function.kind = FunctionKind::Method(other)
    });
}

#[test]
fn foreign_aliases_cannot_issue_owned_resources() {
    let contract = Contract::from_source(
        "foreign.wit",
        "package test:resources; interface types { resource session; } \
         interface factory { use types.{session}; create: func() -> own<session>; } \
         world app { import types; export factory; }",
        Some("app"),
    )
    .unwrap();
    assert!(contract.check_resource_profile().is_err());
}

#[test]
fn capability_data_codec_omits_self_and_preserves_wide_values() {
    use serde_json::json;
    let contract =
        contract("resource session { constructor(seed: u64); add: func(value: s64) -> u64; }");
    let binding = "test:resources/sessions@1.0.0";
    let constructor = contract
        .bind(binding, "[constructor]session", true)
        .unwrap();
    let add = contract.bind(binding, "[method]session.add", true).unwrap();
    assert_eq!(
        contract
            .to_resource_wire(constructor, true, json!([u64::MAX]))
            .unwrap(),
        json!(["18446744073709551615"])
    );
    assert_eq!(
        contract
            .from_resource_wire(constructor, true, json!(["18446744073709551615"]))
            .unwrap(),
        json!([u64::MAX])
    );
    assert_eq!(
        contract
            .to_resource_wire(add, true, json!([i64::MIN]))
            .unwrap(),
        json!(["-9223372036854775808"])
    );
    assert_eq!(
        contract
            .from_resource_wire(add, false, json!("18446744073709551615"))
            .unwrap(),
        json!(u64::MAX)
    );
    assert_eq!(
        contract
            .to_resource_wire(constructor, false, json!(0))
            .unwrap(),
        json!(0)
    );
    for invalid in [json!(-1), json!(4294967296_u64), json!("0"), json!(null)] {
        assert!(
            contract
                .from_resource_wire(constructor, false, invalid)
                .is_err()
        );
    }
    // A forged numeric self cannot enter the data tuple; the caller supplies a typed loan.
    assert!(
        contract
            .to_resource_wire(add, true, json!([0, i64::MIN]))
            .is_err()
    );
    assert!(
        contract
            .from_resource_wire(add, true, json!([i64::MIN]))
            .is_err()
    );
    assert!(contract.to_wire(constructor, false, json!(0)).is_err());
}

#[test]
fn scoped_admission_signatures_do_not_depend_on_type_arena_order() {
    use std::collections::BTreeMap;
    let left = contract("resource session { constructor(); read: func() -> u64; }");
    let right =
        contract("resource padding; resource session { constructor(); read: func() -> u64; }");
    let first = left
        .plan(
            "sessions",
            1,
            &[(
                "test:resources/sessions@1.0.0".into(),
                crate::Binding::Orb {
                    name: "sessions".into(),
                    api_version: 1,
                },
            )]
            .into(),
        )
        .unwrap();
    let second = right
        .plan(
            "sessions",
            1,
            &[(
                "test:resources/sessions@1.0.0".into(),
                crate::Binding::Orb {
                    name: "sessions".into(),
                    api_version: 1,
                },
            )]
            .into(),
        )
        .unwrap();
    assert_eq!(
        first.orb.services[0].signature,
        second.orb.services[0].signature
    );
    assert_eq!(
        first.orb.requires[0].signatures,
        second.orb.requires[0].signatures
    );
    assert!(left.check_profiles(&BTreeMap::new()).is_err());
}

#[test]
fn whole_byte_stream_results_of_async_exports_are_the_only_stream_shape() {
    let frames = contract("frames: async func(count: u64, label: string) -> stream<u8>;");
    frames.check_resource_profile().unwrap();
    let slot = frames
        .bind("test:resources/sessions@1.0.0", "frames", true)
        .unwrap();
    let signature = frames.resource_signature(slot).unwrap();
    assert!(signature.stream_result);
    assert!(signature.receiver.is_none() && !signature.owned_results);
    // Arguments are exact data; the result's data names its one attachment.
    let arguments = frames
        .to_resource_wire(slot, true, serde_json::json!([u64::MAX, "a"]))
        .unwrap();
    assert_eq!(arguments, serde_json::json!(["18446744073709551615", "a"]));
    assert_eq!(
        frames
            .from_resource_wire(slot, false, serde_json::json!(0))
            .unwrap(),
        serde_json::json!(0)
    );
    // The same shape is carried to a compiled importer; data transports never carry streams.
    frames
        .check_import_resource_profile("test:resources/sessions@1.0.0")
        .unwrap();
    let imported = frames
        .bind("test:resources/sessions@1.0.0", "frames", false)
        .unwrap();
    assert!(frames.resource_signature(imported).unwrap().stream_result);
    assert!(frames.check_value_profile().is_err());
    assert!(
        frames
            .check_import_value_profile("test:resources/sessions@1.0.0")
            .is_err()
    );
    for body in [
        "frames: func() -> stream<u8>;",
        "frames: async func() -> stream<u32>;",
        "frames: async func() -> stream;",
        "frames: async func() -> list<stream<u8>>;",
        "frames: async func() -> option<stream<u8>>;",
        "frames: async func(input: stream<u8>) -> u32;",
        "resource session; frames: async func() -> stream<own<session>>;",
        "resource session { frames: async func() -> stream<u8>; }",
    ] {
        let refused = contract(body);
        assert!(refused.check_resource_profile().is_err(), "{body}");
        assert!(
            refused
                .check_import_resource_profile("test:resources/sessions@1.0.0")
                .is_err(),
            "{body}"
        );
    }
}

#[test]
fn owned_results_are_checked_and_mapped_inside_typed_containers() {
    use serde_json::json;
    let contract = contract(
        "resource session; resource other; record bundle { first: session, rest: list<option<session>>, other: result<other, string>, seed: u64 } open: func() -> result<bundle, string>;",
    );
    contract.check_resource_profile().unwrap();
    let slot = contract
        .bind("test:resources/sessions@1.0.0", "open", true)
        .unwrap();
    assert!(contract.resource_signature(slot).unwrap().owned_results);
    let value = json!({"tag":"ok","val":{"first":0,"rest":[1,null,2],"other":{"tag":"ok","val":3},"seed":u64::MAX}});
    let wire = contract
        .to_resource_wire(slot, false, value.clone())
        .unwrap();
    assert_eq!(wire["val"]["seed"], "18446744073709551615");
    assert_eq!(
        contract.from_resource_wire(slot, false, wire).unwrap(),
        value
    );
    let mut rewritten = value;
    let mut names = Vec::new();
    contract
        .map_owned_result(slot, &mut rewritten, |identity, index| {
            names.push((identity.name.clone().unwrap(), index));
            Ok(index + 10)
        })
        .unwrap();
    assert_eq!(
        names,
        [
            ("session".into(), 0),
            ("session".into(), 1),
            ("session".into(), 2),
            ("other".into(), 3)
        ]
    );
    assert_eq!(rewritten["val"]["rest"], json!([11, null, 12]));
    assert!(contract.to_wire(slot, false, rewritten.clone()).is_err());
    let mut malformed = rewritten;
    malformed["val"]["seed"] = json!("wrong");
    let mut called = false;
    assert!(
        contract
            .map_owned_result(slot, &mut malformed, |_, index| {
                called = true;
                Ok(index)
            })
            .is_err()
    );
    assert!(
        !called,
        "all data must be validated before any ownership changes"
    );
    assert!(
        contract
            .from_resource_wire(slot, false, json!({"tag":"err","val":"no"}))
            .is_ok()
    );
}
