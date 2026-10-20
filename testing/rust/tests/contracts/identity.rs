use std::collections::BTreeMap;

use serde_json::{Value, json};
use tembra_contracts::{
    Contract, ContractKind, Member, Placement, ValueSchema, canonical_json, clean_arguments,
    normalize_input, validate,
};

fn local(input: Value) -> Contract {
    Contract::local("test.counter", 1, input, BTreeMap::new()).unwrap()
}

#[test]
fn identity_defaults_cleaning_and_top_level_null_match_acquisition_rules() {
    let schema = json!({
        "type": "object",
        "properties": {
            "key": {"type": "string", "default": "main"},
            "nested": {
                "type": "object",
                "properties": {"n": {"type": "integer", "default": 1}}
            }
        },
        "required": ["key"]
    });
    let contract = local(schema.clone());
    assert_eq!(
        contract.normalize_input(&json!({"extra":1})).unwrap(),
        json!({"key":"main"})
    );
    assert_eq!(
        contract.identity(&json!({})).unwrap(),
        contract.identity(&json!({"key":"main","extra":1})).unwrap()
    );
    assert_eq!(
        contract.identity(&Value::Null).unwrap(),
        contract.identity(&json!({})).unwrap()
    );
    assert_eq!(
        contract.identity(&json!({"nested":{"n":1}})).unwrap(),
        "{\"nested\":{}}"
    );
    assert!(normalize_input(&json!({"type":"null"}), &Value::Null).is_err());
    assert_eq!(
        normalize_input(&json!({}), &json!({"any":"value"})).unwrap(),
        json!({"any":"value"})
    );
    assert_eq!(
        normalize_input(&json!({"type":"object"}), &json!({"any":"value"})).unwrap(),
        json!({})
    );
    assert!(normalize_input(&schema, &json!({"key":1})).is_err());
}

#[test]
fn omitted_false_and_true_policies_remain_distinct_from_validation() {
    for policy in [None, Some(false), Some(true)] {
        let mut schema = json!({
            "type": "object",
            "properties": {"a": {"type": "string"}, "n": {"type": "integer", "default": 7}}
        });
        if let Some(policy) = policy {
            schema["additionalProperties"] = json!(policy);
        }
        let input = json!({"a":"x","extra":1});
        let expected = if policy == Some(true) {
            json!({"a":"x","n":7,"extra":1})
        } else {
            json!({"a":"x","n":7})
        };
        assert_eq!(normalize_input(&schema, &input).unwrap(), expected);
        let cleaned = clean_arguments(&schema, &input).unwrap();
        assert_eq!(cleaned.get("extra").is_some(), policy == Some(true));
        assert!(cleaned.get("n").is_none());
        assert_eq!(validate(&schema, &input).is_ok(), policy != Some(false));
        assert_eq!(input, json!({"a":"x","extra":1}));
    }
}

#[test]
fn nested_null_is_not_defaulted_and_method_null_is_not_acquisition_fallback() {
    let schema = json!({
        "type": "object",
        "properties": {
            "n": {
                "anyOf": [{"type": "null"}, {"type": "string"}],
                "default": "fallback"
            }
        }
    });
    assert_eq!(
        normalize_input(&schema, &json!({"n":null})).unwrap(),
        json!({"n":null})
    );
    assert_eq!(
        normalize_input(&schema, &json!({})).unwrap(),
        json!({"n":"fallback"})
    );
    assert_eq!(
        clean_arguments(&json!({"type":"null"}), &Value::Null).unwrap(),
        Value::Null
    );
    assert!(validate(&json!({"type":"object"}), &Value::Null).is_err());
}

#[test]
fn unsupported_schemas_and_branch_dependent_normalization_fail_closed() {
    for schema in [
        json!({"$ref":"#/$defs/a"}),
        json!({"type":"string","pattern":"a"}),
        json!({"type":"array","items":[{"type":"number"}]}),
        json!({"type":"object","additionalProperties":{"type":"number"}}),
    ] {
        assert!(Contract::local("bad", 1, schema, BTreeMap::new()).is_err());
    }
    let ambiguous = json!({
        "anyOf": [
            {"type": "object", "properties": {"a": {"type": "string"}}},
            {"type": "object", "properties": {"b": {"type": "string"}}}
        ]
    });
    assert_eq!(
        local_result(ambiguous).unwrap_err().reason,
        "ambiguous_normalization"
    );
}

fn local_result(schema: Value) -> Result<Contract, tembra_contracts::ContractError> {
    Contract::local("test", 1, schema, BTreeMap::new())
}

#[test]
fn primitive_array_object_constraints_validate_without_coercion() {
    let schema = json!({
        "type": "object",
        "required": ["items"],
        "properties": {
            "items": {
                "type": "array",
                "minItems": 1,
                "uniqueItems": true,
                "items": {"type": "integer", "minimum": 1, "exclusiveMaximum": 4}
            }
        },
        "additionalProperties": false
    });
    assert!(validate(&schema, &json!({"items":[1,3]})).is_ok());
    for value in [
        json!({"items":[]}),
        json!({"items":[1,1]}),
        json!({"items":[4]}),
        json!({"items":["1"]}),
        json!({"items":[1],"extra":0}),
        json!({}),
    ] {
        assert!(validate(&schema, &value).is_err());
    }
    assert!(
        validate(
            &json!({"type":"string","minLength":1,"maxLength":1}),
            &json!("😀")
        )
        .is_ok()
    );
    assert!(validate(&json!({"type":"number","const":1,"minimum":2}), &json!(1)).is_err());
    assert!(
        validate(
            &json!({"anyOf":[{"type":"number"},{"type":"string"}],"const":true}),
            &json!(true)
        )
        .is_err()
    );
}

#[test]
fn canonical_json_uses_jcs_numbers_unicode_key_order_and_safe_integers() {
    assert_eq!(
        canonical_json(&json!({"z":-0.0,"a":1.5})).unwrap(),
        "{\"a\":1.5,\"z\":0}"
    );
    assert_eq!(
        canonical_json(&json!({"\u{e000}":1,"😀":2})).unwrap(),
        "{\"😀\":2,\"\u{e000}\":1}"
    );
    assert!(canonical_json(&json!(9_007_199_254_740_991_u64)).is_ok());
    assert!(canonical_json(&json!(-9_007_199_254_740_991_i64)).is_ok());
    assert!(canonical_json(&json!(9_007_199_254_740_992_u64)).is_err());
    assert!(canonical_json(&json!(-9_007_199_254_740_992_i64)).is_err());
    assert!(
        validate(
            &json!({"type":"number","minimum":9_007_199_254_740_992_u64}),
            &json!(1)
        )
        .is_err()
    );
}

#[test]
fn native_metadata_and_fingerprints_round_trip_without_native_values() {
    let members = BTreeMap::from([
        (
            "echo".into(),
            Member::Method {
                args: Some(ValueSchema::Native("request@1".into())),
                result: Some(ValueSchema::Native("response@1".into())),
            },
        ),
        (
            "native".into(),
            Member::Opaque {
                id: "capability@1".into(),
            },
        ),
    ]);
    let contract =
        Contract::local("test.native", 1, json!({"type":"object"}), members.clone()).unwrap();
    assert_eq!(contract.kind(), ContractKind::Local);
    assert_eq!(contract.fingerprint().len(), 64);
    assert_eq!(
        serde_json::from_value::<Contract>(serde_json::to_value(&contract).unwrap()).unwrap(),
        contract
    );
    let mut tampered = serde_json::to_value(&contract).unwrap();
    tampered["version"] = json!(2);
    assert!(serde_json::from_value::<Contract>(tampered).is_err());
    assert!(
        Contract::remote(
            "test.native",
            1,
            json!({}),
            Placement::Named {
                name: "service".into()
            },
            BTreeMap::from([("main".into(), members)])
        )
        .is_err()
    );
}

#[test]
fn descriptor_boundaries_reject_unknown_fields_reserved_names_and_wrong_placement() {
    let contract = local(json!({}));
    let mut descriptor = serde_json::to_value(&contract).unwrap();
    descriptor["unknown"] = json!(1);
    assert!(serde_json::from_value::<Contract>(descriptor).is_err());
    for name in ["then", "release", ""] {
        assert!(
            Contract::local(
                "test",
                1,
                json!({}),
                BTreeMap::from([(name.into(), Member::Opaque { id: "cap@1".into() })])
            )
            .is_err()
        );
    }
    assert!(serde_json::from_value::<Placement>(json!({"name":"service","extra":1})).is_err());
    let input = json!({
        "type": "object",
        "properties": {"workspace": {"type": "string"}},
        "required": ["workspace"]
    });
    let remote = Contract::remote(
        "test.remote",
        1,
        input,
        Placement::Family {
            prefix: "workspace:".into(),
            key: "workspace".into(),
        },
        BTreeMap::from([("main".into(), BTreeMap::new())]),
    )
    .unwrap();
    assert_eq!(
        remote.resolve_host(&json!({"workspace":"one"})).unwrap(),
        Some("workspace:one".into())
    );
    assert!(remote.resolve_host(&json!({"workspace":""})).is_err());
    assert!(serde_json::from_value::<Member>(json!({"kind":"method","args":null})).is_ok());
    let member: Member = serde_json::from_value(json!({"kind":"method","args":null})).unwrap();
    assert!(
        Contract::local(
            "test",
            1,
            json!({}),
            BTreeMap::from([("call".into(), member)])
        )
        .is_err()
    );
}
