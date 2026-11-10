//! Resource names survive preparation independently of arena order and import aliases.
use super::*;

#[test]
fn resource_aliases_keep_the_defining_interface_and_name() {
    let contract = Contract::from_source(
        "sessions.wit",
        r#"
            package example:sessions@1.0.0;
            interface decoders {
                resource decoder {
                    constructor();
                    feed: func(value: u64) -> u64;
                }
            }
            interface relay {
                use decoders.{decoder as session};
                read: func(value: borrow<session>) -> u64;
            }
            world app { import decoders; export relay; }
        "#,
        Some("app"),
    )
    .unwrap();
    let identity = ResourceIdentity {
        interface: Some("example:sessions/decoders@1.0.0".into()),
        name: Some("decoder".into()),
    };
    let encoded = PreparedContract::encode(&contract);
    let decoded = PreparedContract::decode(&encoded).unwrap();
    let mut aliases = 0;
    for (id, kind) in decoded.model().types.iter().enumerate() {
        if let Kind::Alias(_) = kind
            && let Some(actual) = decoded.resource_identity(Ty::Ref(id as u32))
        {
            assert_eq!(actual, &identity);
            aliases += 1;
        }
        if let Kind::Borrow(resource) | Kind::Own(resource) = kind {
            assert_eq!(
                decoded.resource_identity(Ty::Ref(*resource)),
                Some(&identity)
            );
        }
    }
    assert!(aliases > 0, "the renamed use must resolve through an alias");
    assert_eq!(decoded.resource_identity(Ty::Ref(u32::MAX)), None);
    assert_eq!(decoded.resource_identity(Ty::String), None);
    assert_eq!(decoded.encode(), encoded);
    let Item::Interface(relay) = &decoded.model().exports[0].item else {
        panic!("expected the relay interface")
    };
    assert!(relay.resources.contains_key("session"));
    assert!(!relay.resources.contains_key("decoder"));
    assert_eq!(
        decoded.resource_identity(Ty::Ref(relay.resources["session"])),
        Some(&identity)
    );
    // Identity metadata alone must not admit a handle to the portable value profile.
    assert!(decoded.check_value_profile().is_err());
}

#[test]
fn resource_only_interfaces_keep_distinct_nominal_types() {
    let contract = Contract::from_source(
        "types.wit",
        r#"
            package example:types@1.0.0;
            interface left { resource session; }
            interface right { resource session; }
            world app { import left; import right; }
        "#,
        Some("app"),
    )
    .unwrap();
    let decoded = PreparedContract::decode(&PreparedContract::encode(&contract)).unwrap();
    let identities = decoded
        .model()
        .types
        .iter()
        .filter_map(|kind| match kind {
            Kind::Resource(identity) => Some(identity),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(identities.len(), 2);
    assert_eq!(identities[0].name, identities[1].name);
    assert_ne!(identities[0].interface, identities[1].interface);
    assert_eq!(decoded, *contract);
    let described = decoded.describe();
    let resources = described["types"].as_array().unwrap();
    assert_eq!(resources[0]["definition"]["name"], "session");
    assert_eq!(
        resources[0]["definition"]["owner"],
        "example:types/left@1.0.0"
    );
}

#[test]
fn anonymous_resources_remain_inspectable_without_fabricating_a_nominal_owner() {
    let contract = Contract::from_source(
        "inline.wit",
        r#"
            package example:inline@1.0.0;
            world app { import local: interface { resource session; } }
        "#,
        Some("app"),
    )
    .unwrap();
    let decoded = PreparedContract::decode(&PreparedContract::encode(&contract)).unwrap();
    assert_eq!(
        decoded.resource_identity(Ty::Ref(0)),
        Some(&ResourceIdentity {
            interface: None,
            name: Some("session".into()),
        })
    );
}

#[test]
fn malformed_resource_identity_and_old_compact_format_are_refused() {
    let contract = Contract::from_source(
        "types.wit",
        "package example:types; interface types { resource session; } world app { import types; }",
        Some("app"),
    )
    .unwrap();
    let mut model = contract.model().clone();
    let Kind::Resource(resource) = &mut model.types[0] else {
        panic!("resource-only interface must retain its type")
    };
    resource.name = None;
    assert!(PreparedContract::new(model).is_err());
    let mut duplicate = contract.model().clone();
    duplicate.types.push(duplicate.types[0].clone());
    assert!(
        PreparedContract::new(duplicate)
            .unwrap_err()
            .0
            .contains("duplicate nominal")
    );
    let mut encoded = PreparedContract::encode(&contract);
    assert_eq!(encoded[0], 3);
    encoded[0] = 2;
    assert!(PreparedContract::decode(&encoded).is_err());

    let mut invalid = contract.model().clone();
    let Item::Interface(interface) = &mut invalid.imports[0].item else {
        panic!("expected the types interface")
    };
    interface.resources.insert("session".into(), u32::MAX);
    assert!(PreparedContract::new(invalid).is_err());

    for change_owner in [false, true] {
        let mut invalid = contract.model().clone();
        let Kind::Resource(resource) = &mut invalid.types[0] else {
            panic!("expected the resource definition")
        };
        if change_owner {
            resource.interface = Some("other:types/types".into());
        } else {
            resource.name = Some("counterfeit".into());
        }
        assert!(PreparedContract::new(invalid).is_err());
    }
}

#[test]
fn resource_signatures_compare_nominal_identity_and_ownership_instead_of_indexes() {
    let contract = Contract::from_source(
        "identity.wit",
        r#"
            package example:identity@1.0.0;
            interface types {
                resource session;
                inspect: func(value: borrow<session>) -> u64;
                take: func(value: own<session>) -> u64;
            }
            interface relay {
                use types.{session as renamed};
                inspect: func(value: borrow<renamed>) -> u64;
            }
            interface other {
                resource session;
                inspect: func(value: borrow<session>) -> u64;
            }
            world app { export types; export relay; export other; }
        "#,
        Some("app"),
    )
    .unwrap();
    let signature = |interface: &str, method: &str| {
        contract
            .function_signature(&format!("example:identity/{interface}@1.0.0"), method, true)
            .unwrap()
    };
    assert_eq!(signature("types", "inspect"), signature("relay", "inspect"));
    assert_ne!(signature("types", "inspect"), signature("other", "inspect"));
    assert_ne!(signature("types", "inspect"), signature("types", "take"));
    assert!(contract.check_value_profile().is_err());
}
