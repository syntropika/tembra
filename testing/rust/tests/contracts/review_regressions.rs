use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Value, json};
use tembra_contracts::{
    Contract, ContractBuilder, Member, ValueSchema, canonical_json, clean_arguments,
    identity_compatible, input_compatible, normalize_input, normalize_schema, output_fits,
    parse_json, schema_for, validate,
};

#[test]
fn sibling_union_cleaning_refuses_without_losing_fields_or_restricting_raw_outputs() {
    let schema = json!({
        "type": "object",
        "properties": {"a": {"type": "string"}},
        "anyOf": [{"type": "object"}]
    });
    let value = json!({"a":"x"});
    let before = value.clone();
    for error in [
        normalize_input(&schema, &value).unwrap_err(),
        clean_arguments(&schema, &value).unwrap_err(),
    ] {
        assert_eq!(error.reason, "ambiguous_normalization");
        assert_eq!(error.path, "/anyOf");
    }
    assert_eq!(value, before);
    validate(&schema, &value).unwrap();
    let output = Contract::local(
        "output",
        1,
        json!({}),
        BTreeMap::from([(
            "read".into(),
            Member::Method {
                args: None,
                result: Some(ValueSchema::Portable(schema.clone())),
            },
        )]),
    )
    .unwrap();
    assert!(output.members("").is_some());
    let nested = json!({"type":"object","properties":{"value":schema}});
    let error = clean_arguments(&nested, &json!({"value":{"a":"x"}})).unwrap_err();
    assert_eq!(error.path, "/properties/value/anyOf");
}

#[test]
fn open_object_extras_cannot_preserve_a_min_properties_proof_after_cleaning() {
    let consumer = json!({
        "type": "object",
        "properties": {"a": {"type": "string"}},
        "additionalProperties": true,
        "minProperties": 2
    });
    let mut provider = consumer.clone();
    provider
        .as_object_mut()
        .unwrap()
        .remove("additionalProperties");
    let source = json!({"a":"x","z":1});
    let sent = clean_arguments(&consumer, &source).unwrap();
    validate(&consumer, &sent).unwrap();
    let received = clean_arguments(&provider, &sent).unwrap();
    assert_eq!(received, json!({"a":"x"}));
    assert!(validate(&provider, &received).is_err());
    assert!(input_compatible(&consumer, &provider).is_err());
    provider.as_object_mut().unwrap().remove("minProperties");
    input_compatible(&consumer, &provider).unwrap();
}

#[test]
fn strict_json_accepts_exactly_128_containers_and_normalizes_integral_storage() {
    for leaf in ["0", "\"[]{}\""] {
        let at_limit = format!("{}{}{}", "[".repeat(128), leaf, "]".repeat(128));
        let value = parse_json(&at_limit).unwrap();
        assert_eq!(canonical_json(&value).unwrap(), at_limit);
        let excessive = format!("[{at_limit}]");
        assert_eq!(parse_json(&excessive).unwrap_err().reason, "depth");
    }
    let empty_at_limit = format!("{}{}", "[".repeat(128), "]".repeat(128));
    parse_json(&empty_at_limit).unwrap();
    assert!(parse_json(&format!("[{empty_at_limit}]")).is_err());
    assert_eq!(parse_json("[1.0,1e0,-0.0]").unwrap(), json!([1, 1, 0]));
    let mut too_deep = json!([]);
    for _ in 0..128 {
        too_deep = json!([too_deep]);
    }
    assert!(canonical_json(&too_deep).is_err());
}

#[test]
fn both_peer_decoders_require_digests_and_accept_float_spelled_safe_headers() {
    let authored = Contract::local("headers", 1, json!({}), BTreeMap::new()).unwrap();
    let text = serde_json::to_string(&authored).unwrap();
    let floating = text
        .replace("\"descriptorVersion\":1", "\"descriptorVersion\":1e0")
        .replace("\"version\":1", "\"version\":1.0");
    assert_eq!(Contract::from_json(&floating).unwrap(), authored);
    assert_eq!(
        serde_json::from_str::<Contract>(&floating).unwrap(),
        authored
    );
    let mut missing = serde_json::to_value(&authored).unwrap();
    missing.as_object_mut().unwrap().remove("fingerprint");
    assert!(Contract::from_json(&missing.to_string()).is_err());
    assert!(serde_json::from_value::<Contract>(missing).is_err());
    let mut forged = serde_json::to_value(&authored).unwrap();
    forged["version"] = json!(2.0);
    assert!(Contract::from_json(&forged.to_string()).is_err());
    assert!(serde_json::from_value::<Contract>(forged).is_err());
    let mut draft = authored.descriptor().clone();
    draft.fingerprint.clear();
    assert_eq!(Contract::from_descriptor(draft).unwrap(), authored);
}

#[test]
fn canonical_numeric_schema_storage_preserves_cross_language_widening_and_defaults() {
    let provider = schema_for::<i32>().unwrap();
    let consumer = json!({
        "type": "number",
        "minimum": -2147483648,
        "maximum": 2147483647
    });
    output_fits(&provider, &consumer).unwrap();
    input_compatible(
        &json!({"type":"integer","minimum":0.0,"maximum":10.0}),
        &json!({"type":"integer","minimum":0,"maximum":10}),
    )
    .unwrap();
    identity_compatible(
        &json!({"type":"object","properties":{"count":{"type":"integer","default":1}}}),
        &json!({"type":"object","properties":{"count":{"type":"integer","default":1.0}}}),
    )
    .unwrap();
    assert_eq!(
        normalize_schema(&json!({"minimum":1.0})).unwrap(),
        json!({"minimum":1})
    );
}

#[derive(JsonSchema, Serialize)]
struct Child {
    count: u32,
}

#[derive(JsonSchema, Serialize)]
struct NullableInput {
    child: Option<Child>,
    amount: Option<f64>,
}

#[test]
fn common_schemars_nullable_scalars_and_objects_normalize_without_null_coercion() {
    let unsigned = schema_for::<Option<u32>>().unwrap();
    validate(&unsigned, &Value::Null).unwrap();
    validate(&unsigned, &json!(4294967295_u64)).unwrap();
    assert!(validate(&unsigned, &json!(-1)).is_err());
    assert!(validate(&unsigned, &json!(4294967296_u64)).is_err());
    let floating = schema_for::<Option<f64>>().unwrap();
    validate(&floating, &Value::Null).unwrap();
    validate(&floating, &json!(1.5)).unwrap();
    let schema = schema_for::<NullableInput>().unwrap();
    let input = json!({"child":{"count":7,"extra":true},"amount":null,"extra":0});
    let normalized = json!({"child":{"count":7},"amount":null});
    assert_eq!(normalize_input(&schema, &input).unwrap(), normalized);
    assert_eq!(clean_arguments(&schema, &input).unwrap(), normalized);
    assert_eq!(
        normalize_input(&schema, &json!({"child":null,"amount":null})).unwrap(),
        json!({"child":null,"amount":null})
    );
    for types in [json!(["object", "null"]), json!(["null", "object"])] {
        let nullable = json!({
            "type":types,
            "properties":{"count":{"type":"integer"}},
            "required":["count"]
        });
        assert_eq!(
            clean_arguments(&nullable, &json!({"count":3,"extra":1})).unwrap(),
            json!({"count":3})
        );
        assert_eq!(
            clean_arguments(&nullable, &Value::Null).unwrap(),
            Value::Null
        );
    }
    let distinct_objects = json!({"anyOf":[
        {"type":"object","properties":{"a":{"type":"integer"}}},
        {"type":"object","properties":{"b":{"type":"integer"}}}
    ]});
    assert_eq!(
        clean_arguments(&distinct_objects, &json!({}))
            .unwrap_err()
            .reason,
        "ambiguous_normalization"
    );
}

#[test]
fn builder_default_input_refuses_nonfinite_nullable_values_before_null_encoding() {
    let schema = schema_for::<NullableInput>().unwrap();
    for amount in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let input = NullableInput {
            child: None,
            amount: Some(amount),
        };
        let error = ContractBuilder::local("finite", 1)
            .input_schema(schema.clone())
            .default_input(&input)
            .unwrap_err();
        assert_eq!(error.reason, "serialization");
        if amount.is_nan() {
            assert!(input.amount.unwrap().is_nan());
        } else {
            assert_eq!(input.amount, Some(amount));
        }
    }
    assert_eq!(
        ContractBuilder::local("finite", 1)
            .input_schema(schema)
            .default_input(&NullableInput {
                child: None,
                amount: Some(1.5)
            })
            .unwrap(),
        json!({"child":null,"amount":1.5})
    );
}
