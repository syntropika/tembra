use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Value, json};
use tembra_contracts::{
    CompileLimits, Contract, ContractBuilder, Member, Placement, ValueSchema, canonical_json,
    clean_arguments, identity_compatible, input_compatible, normalize_input, normalize_schema,
    normalize_schema_with_limits, output_fits, parse_json, schema_for, validate,
};

#[test]
fn shared_cross_language_vectors_are_executed_in_rust() {
    // The corpus deliberately contains invalid data, so it is decoded as test data,
    // not through the strict portable-value ingestion boundary being exercised.
    let fixtures: Value =
        serde_json::from_str(include_str!("../../../fixtures/contracts/v1.json")).unwrap();
    for fixture in fixtures["descriptors"].as_array().unwrap() {
        let contract = Contract::from_json(&fixture["descriptor"].to_string()).unwrap();
        assert_eq!(
            contract.fingerprint(),
            fixture["fingerprint"].as_str().unwrap()
        );
        let mut descriptor = serde_json::to_value(contract).unwrap();
        descriptor.as_object_mut().unwrap().remove("fingerprint");
        assert_eq!(
            canonical_json(&descriptor).unwrap(),
            fixture["canonical"].as_str().unwrap()
        );
    }
    for fixture in fixtures["validation"].as_array().unwrap() {
        assert_eq!(
            validate(&fixture["schema"], &fixture["value"]).is_ok(),
            fixture["valid"].as_bool().unwrap(),
            "{}",
            fixture["name"]
        );
    }
    for fixture in fixtures["normalization"].as_array().unwrap() {
        let contract = Contract::local(
            "fixture.identity",
            1,
            fixture["schema"].clone(),
            BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(
            contract.normalize_input(&fixture["input"]).unwrap(),
            fixture["normalized"]
        );
        assert_eq!(
            contract.identity(&fixture["input"]).unwrap(),
            fixture["identity"].as_str().unwrap()
        );
    }
    for fixture in fixtures["canonical"].as_array().unwrap() {
        assert_eq!(
            canonical_json(&fixture["value"]).unwrap(),
            fixture["bytes"].as_str().unwrap()
        );
    }
    for fixture in fixtures["rawJson"].as_array().unwrap() {
        assert_eq!(
            parse_json(fixture["text"].as_str().unwrap()).is_ok(),
            fixture["valid"].as_bool().unwrap(),
            "{}",
            fixture["name"]
        );
    }
}

#[test]
fn local_references_inline_repeated_escaped_and_nested_definitions() {
    let schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "$defs": {
            "a/b~c": {"type": "integer"},
            "outer": {"type": "array", "items": {"$ref": "#/$defs/a~1b~0c"}}
        },
        "properties": {
            "first": {"$ref": "#/$defs/outer", "description": "first"},
            "second": {"$ref": "#/$defs/outer"}
        }
    });
    let normalized = normalize_schema(&schema).unwrap();
    assert!(normalized.get("$defs").is_none());
    assert!(normalized.get("$schema").is_none());
    assert_eq!(
        normalized["properties"]["first"],
        json!({"type":"array","items":{"type":"integer"},"description":"first"})
    );
    assert_eq!(
        normalized["properties"]["second"],
        json!({"type":"array","items":{"type":"integer"}})
    );
    assert!(validate(&normalized, &json!({"first":[1],"second":[2]})).is_ok());
    assert_eq!(
        normalize_schema(&json!({"$defs":{"target":{"type":"integer"}},"$ref":"#/$defs/target"}))
            .unwrap(),
        json!({"type":"integer"})
    );
}

#[test]
fn reference_cycles_siblings_resources_and_expansion_limits_fail_closed() {
    assert_eq!(
        normalize_schema(&json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$defs": {"target": false},
            "$ref": "#/$defs/target"
        }))
        .unwrap(),
        json!(false)
    );
    for schema in [
        json!({"$defs":{"self":{"$ref":"#/$defs/self"}},"$ref":"#/$defs/self"}),
        json!({
            "$defs": {"a": {"$ref": "#/$defs/b"}, "b": {"$ref": "#/$defs/a"}},
            "type": "object"
        }),
        json!({"$ref":"https://example.invalid/schema"}),
        json!({
            "$defs": {"x": {"type": "string"}},
            "properties": {"x": {"$ref": "#/$defs/x", "minLength": 1}}
        }),
        json!({"$id":"relative","type":"string"}),
        json!({"$ref":"#/$defs/missing"}),
        json!({"$defs":{"bad":{"format":"unknown"}},"type":"string"}),
        json!({
            "$defs": {"x": {"type": "string"}},
            "properties": {"x": {"$ref": "#/$defs/x~2"}}
        }),
    ] {
        assert!(normalize_schema(&schema).is_err(), "{schema}");
    }
    assert_eq!(
        normalize_schema_with_limits(
            &json!({"type":"array","items":{"type":"string"}}),
            CompileLimits {
                max_depth: 0,
                max_nodes: 10
            }
        )
        .unwrap_err()
        .reason,
        "schema_budget"
    );
    assert!(
        normalize_schema_with_limits(
            &json!({"properties":{"a":{},"b":{}}}),
            CompileLimits {
                max_depth: 10,
                max_nodes: 2
            }
        )
        .is_err()
    );
}

#[test]
fn peer_decoding_requires_and_verifies_digest_and_strict_json() {
    let contract = Contract::local("test.peer", 1, json!({}), BTreeMap::new()).unwrap();
    let value = serde_json::to_value(&contract).unwrap();
    Contract::from_json(&value.to_string()).unwrap();
    for digest in [
        json!(""),
        json!("A".repeat(64)),
        json!("0".repeat(64)),
        Value::Null,
    ] {
        let mut invalid = value.clone();
        invalid["fingerprint"] = digest;
        assert!(Contract::from_json(&invalid.to_string()).is_err());
    }
    let mut missing = value.clone();
    missing.as_object_mut().unwrap().remove("fingerprint");
    assert!(Contract::from_json(&missing.to_string()).is_err());
    for input in [
        "{\"a\":1,\"a\":2}",
        "{\"a\":{\"x\":1,\"x\":2}}",
        "\"\\ud800\"",
        "9007199254740993",
        "900719925474099300e-2",
        "9007199254740991.1",
        "1e-999",
        "-9007199254740993",
        "1e100",
        "{} true",
    ] {
        assert!(parse_json(input).is_err(), "{input}");
    }
    assert_eq!(parse_json("\"\\ud83d\\ude00\"").unwrap(), json!("😀"));
    assert_eq!(
        canonical_json(&parse_json("900719925474099100e-2").unwrap()).unwrap(),
        "9007199254740991"
    );
}

#[test]
fn lexical_number_checks_ignore_strings_and_preserve_mathematical_zero() {
    for input in [
        r#""1e999""#,
        r#"{"9007199254740993":"1e-999"}"#,
        r#""escaped\"9007199254740991.1""#,
        r#""\u0031e999""#,
    ] {
        assert!(parse_json(input).is_ok(), "{input}");
    }
    for input in ["-0.000", "0e-999", "-0.0e999", "0.0000"] {
        assert_eq!(
            canonical_json(&parse_json(input).unwrap()).unwrap(),
            "0",
            "{input}"
        );
    }
    assert_eq!(
        canonical_json(&parse_json("333333333.33333329").unwrap()).unwrap(),
        "333333333.3333333"
    );
}

#[test]
fn mathematically_integral_count_bounds_and_explicit_object_policies_normalize() {
    assert_eq!(
        normalize_schema(&json!({"type":"string","minLength":1.0})).unwrap(),
        json!({"type":"string","minLength":1})
    );
    assert!(validate(&json!({"type":"string","minLength":1.0}), &json!("x")).is_ok());
    assert_eq!(
        normalize_input(&json!({"additionalProperties":false}), &json!({"extra":1})).unwrap(),
        json!({})
    );
}

#[derive(Serialize, JsonSchema)]
struct CounterInput {
    name: String,
}
#[derive(Serialize, JsonSchema)]
struct Increment {
    by: i32,
}

#[test]
fn typed_rust_origin_rebuilds_the_independent_shared_descriptor() {
    let expected =
        Contract::from_json(include_str!("../../../fixtures/contracts/typed-rust.json")).unwrap();
    let actual = ContractBuilder::remote("fixture.typed-counter", 1)
        .input::<CounterInput>()
        .unwrap()
        .placement(Placement::Named {
            name: "service".into(),
        })
        .select_variant("default")
        .typed_method::<Increment, i32>("increment")
        .unwrap()
        .typed_query::<i32>("read")
        .unwrap()
        .typed_state::<i32>("count")
        .unwrap()
        .method("touch", None, None)
        .build()
        .unwrap();
    assert_eq!(actual, expected);
    assert_eq!(actual.fingerprint(), expected.fingerprint());
}

#[test]
fn output_covariance_rejects_both_demonstrated_chord_holes() {
    let provider = json!({"type":"object","properties":{"a":{"type":"string"}}});
    let consumer = json!({
        "type": "object",
        "properties": {"a": {"type": "string"}, "b": {"type": "number"}}
    });
    let witness = json!({"a":"x","b":"wrong"});
    validate(&provider, &witness).unwrap();
    assert!(validate(&consumer, &witness).is_err());
    assert!(output_fits(&provider, &consumer).is_err());
    assert!(
        output_fits(
            &json!({"type":"array","items":[{"type":"number"}],"additionalItems":true}),
            &json!({"type":"array","items":[{"type":"number"}],"additionalItems":false})
        )
        .is_err()
    );
    assert!(
        output_fits(
            &json!({
                "type": "object",
                "properties": {"a": {"type": "string"}},
                "additionalProperties": false
            }),
            &consumer
        )
        .is_ok()
    );
}

#[test]
fn finite_coverage_checks_every_sibling_constraint_and_union_direction() {
    assert!(output_fits(&json!({"const":1}), &json!({"const":1,"type":"string"})).is_err());
    assert!(
        output_fits(
            &json!({"enum":[1,2]}),
            &json!({"type":"number","minimum":2})
        )
        .is_err()
    );
    assert!(output_fits(&json!({"const":2}), &json!({"type":"number","minimum":2})).is_ok());
    assert!(output_fits(&json!({"type":"integer"}), &json!({"type":"number"})).is_ok());
    assert!(output_fits(&json!({"type":"number"}), &json!({"type":"integer"})).is_err());
    assert!(
        output_fits(
            &json!({"type":"string"}),
            &json!({"anyOf":[{"type":"string"},{"type":"number"}]})
        )
        .is_ok()
    );
    assert!(
        output_fits(
            &json!({"anyOf":[{"type":"string"},{"type":"number"}]}),
            &json!({"type":"string"})
        )
        .is_err()
    );
    assert!(
        output_fits(
            &json!({"type":"number","minimum":2}),
            &json!({"type":"number","minimum":1})
        )
        .is_err()
    );
}

#[test]
fn argument_projection_allows_additions_without_reusing_output_assumptions() {
    let old = json!({
        "type": "object",
        "properties": {"a": {"type": "string"}},
        "required": ["a"]
    });
    let optional = json!({
        "type": "object",
        "properties": {"a": {"type": "string"}, "b": {"type": "number"}},
        "required": ["a"]
    });
    assert!(input_compatible(&old, &optional).is_ok());
    assert!(output_fits(&old, &optional).is_err());
    let mut required = optional.clone();
    required["required"] = json!(["a", "b"]);
    assert!(input_compatible(&old, &required).is_err());
    let mut open = old.clone();
    open["additionalProperties"] = json!(true);
    assert!(input_compatible(&open, &optional).is_err());
    assert!(identity_compatible(&old, &optional).is_err());
    let mut changed_default = old.clone();
    changed_default["properties"]["a"]["default"] = json!("changed");
    assert!(identity_compatible(&old, &changed_default).is_err());
}

fn method(args: Option<ValueSchema>, result: Option<ValueSchema>) -> Member {
    Member::Method { args, result }
}

#[test]
fn contracts_cover_additive_members_presence_and_nominal_ids_but_not_versions() {
    let old = Contract::local(
        "test",
        1,
        json!({}),
        BTreeMap::from([("call".into(), method(None, None))]),
    )
    .unwrap();
    let added = Contract::local(
        "test",
        1,
        json!({}),
        BTreeMap::from([
            (
                "call".into(),
                method(None, Some(ValueSchema::Portable(json!({"type":"integer"})))),
            ),
            ("extra".into(), method(None, None)),
        ]),
    )
    .unwrap();
    added.covers(&old).unwrap();
    assert!(old.covers(&added).is_err());
    let different = Contract::local(
        "test",
        2,
        json!({}),
        BTreeMap::from([("call".into(), method(None, None))]),
    )
    .unwrap();
    assert!(different.covers(&old).is_err());
    let native = |id: &str| {
        Contract::local(
            "native",
            1,
            json!({}),
            BTreeMap::from([(
                "call".into(),
                method(Some(ValueSchema::Native(id.into())), None),
            )]),
        )
        .unwrap()
    };
    native("request@1").covers(&native("request@1")).unwrap();
    assert!(native("request@2").covers(&native("request@1")).is_err());
    let takes_args = Contract::local(
        "test",
        1,
        json!({}),
        BTreeMap::from([(
            "call".into(),
            method(Some(ValueSchema::Portable(json!({"type":"null"}))), None),
        )]),
    )
    .unwrap();
    assert!(takes_args.covers(&old).is_err());
}

#[derive(JsonSchema, Serialize)]
struct Input {
    label: String,
}
#[derive(JsonSchema, Serialize)]
struct Command {
    count: u32,
}

struct UnsupportedFormat;
impl JsonSchema for UnsupportedFormat {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "UnsupportedFormat".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::Schema::try_from(json!({"type":"string","format":"email"})).unwrap()
    }
}

struct InvalidNumericBound;
impl JsonSchema for InvalidNumericBound {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "InvalidNumericBound".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::Schema::try_from(json!({"type":"integer","format":"int32","minimum":"invalid"}))
            .unwrap()
    }
}

#[test]
fn schemars_adaptation_does_not_silently_drop_unknown_or_invalid_validation() {
    assert_eq!(
        schema_for::<UnsupportedFormat>().unwrap_err().reason,
        "unsupported_format"
    );
    assert_eq!(
        schema_for::<InvalidNumericBound>().unwrap_err().reason,
        "schema"
    );
    let schema = schema_for::<u64>().unwrap();
    assert!(validate(&schema, &json!(9_007_199_254_740_991_u64)).is_ok());
    assert!(validate(&schema, &json!(9_007_199_254_740_992_u64)).is_err());
}

#[test]
fn typed_rust_builder_describes_members_and_adapts_known_numeric_storage_formats() {
    let contract = ContractBuilder::remote("test.typed", 1)
        .input::<Input>()
        .unwrap()
        .placement(Placement::Named {
            name: "service".into(),
        })
        .typed_command::<Command>("write")
        .unwrap()
        .typed_query::<i32>("read")
        .unwrap()
        .typed_state::<u32>("count")
        .unwrap()
        .build()
        .unwrap();
    assert_eq!(contract.input()["title"], "Input");
    assert_eq!(contract.members("main").unwrap().len(), 3);
    let Member::Method {
        args: Some(ValueSchema::Portable(args)),
        ..
    } = &contract.members("main").unwrap()["write"]
    else {
        panic!("missing typed command")
    };
    assert_eq!(args["properties"]["count"]["minimum"], json!(0));
    assert_eq!(
        args["properties"]["count"]["maximum"],
        json!(4294967295_u64)
    );
    assert!(args["properties"]["count"].get("format").is_none());
    assert!(validate(&schema_for::<i32>().unwrap(), &json!(2147483648_u64)).is_err());
    assert!(
        ContractBuilder::local("duplicate", 1)
            .method("m", None, None)
            .method("m", None, None)
            .build()
            .is_err()
    );
    assert_eq!(
        normalize_input(contract.input(), &json!({"label":"a","extra":1})).unwrap(),
        json!({"label":"a"})
    );
}

#[test]
fn successful_output_proofs_hold_for_a_small_independent_value_corpus() {
    let schemas = vec![
        json!({}),
        json!(false),
        json!({"type":"integer"}),
        json!({"type":"number"}),
        json!({"type":"string"}),
        json!({"const":1}),
        json!({"enum":[1,2]}),
        json!({"type":"object","properties":{"a":{"type":"string"}}}),
        json!({
            "type": "object",
            "properties": {"a": {"type": "string"}},
            "additionalProperties": false
        }),
        json!({
            "type": "object",
            "properties": {"a": {"type": "string"}, "b": {"type": "number"}}
        }),
        json!({"type":"array","items":{"type":"integer"}}),
    ];
    let values = vec![
        json!(null),
        json!(true),
        json!(1),
        json!(1.5),
        json!(2),
        json!("x"),
        json!({}),
        json!({"a":"x"}),
        json!({"a":"x","b":"bad"}),
        json!({"a":"x","b":1}),
        json!([]),
        json!([1]),
        json!(["x"]),
    ];
    for provider in &schemas {
        for consumer in &schemas {
            if output_fits(provider, consumer).is_ok() {
                for value in &values {
                    if validate(provider, value).is_ok() {
                        assert!(
                            validate(consumer, value).is_ok(),
                            "invalid proof {provider} => {consumer} with {value}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn input_proofs_account_for_projection_collapsing_unique_items() {
    let consumer = json!({
        "type": "array",
        "uniqueItems": true,
        "items": {
            "type": "object",
            "properties": {"a": {"type": "string"}, "b": {"type": "integer"}}
        }
    });
    let provider = json!({
        "type": "array",
        "uniqueItems": true,
        "items": {"type": "object", "properties": {"a": {"type": "string"}}}
    });
    let value = json!([{"a":"same","b":1},{"a":"same","b":2}]);
    validate(&consumer, &value).unwrap();
    let cleaned = clean_arguments(&provider, &clean_arguments(&consumer, &value).unwrap()).unwrap();
    assert!(validate(&provider, &cleaned).is_err());
    assert!(input_compatible(&consumer, &provider).is_err());
    let consumer = json!({
        "type": "object",
        "properties": {
            "item": {
                "type": "object",
                "properties": {"a": {"type": "integer"}, "b": {"type": "integer"}},
                "enum": [{"a": 1, "b": 1}, {"a": 1, "b": 2}]
            }
        }
    });
    let provider = json!({
        "type": "object",
        "properties": {"item": {"type": "object", "properties": {"a": {"type": "string"}}}}
    });
    assert!(input_compatible(&consumer, &provider).is_err());
}

#[test]
fn successful_input_proofs_hold_after_both_cleaning_passes() {
    let a = json!({"type":"object","properties":{"a":{"type":"string"}}});
    let b = json!({
        "type": "object",
        "properties": {"a": {"type": "string"}, "b": {"type": "integer"}}
    });
    let schemas = vec![
        json!({}),
        json!({"type":"integer"}),
        json!({"type":"number"}),
        a.clone(),
        b.clone(),
        json!({
            "type": "object",
            "properties": {"a": {"type": "string"}},
            "additionalProperties": true
        }),
        json!({"type":"array","items":a}),
        json!({"type":"array","items":b}),
    ];
    let values = vec![
        json!(null),
        json!(1),
        json!(1.5),
        json!({}),
        json!({"a":"x"}),
        json!({"a":"x","b":1}),
        json!({"a":"x","b":"wrong"}),
        json!({"unknown":1}),
        json!([]),
        json!([{"a":"x","b":1}]),
    ];
    for consumer in &schemas {
        for provider in &schemas {
            if input_compatible(consumer, provider).is_ok() {
                for value in &values {
                    let sent = clean_arguments(consumer, value).unwrap();
                    if validate(consumer, &sent).is_ok() {
                        let received = clean_arguments(provider, &sent).unwrap();
                        assert!(
                            validate(provider, &received).is_ok(),
                            "invalid input proof {consumer} -> {provider} with {value}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn reference_chains_preserve_boolean_targets_and_nested_pointer_indices() {
    for terminal in [false, true] {
        let source = json!({
            "$defs": {"F": terminal, "Alias": {"$ref": "#/$defs/F"}},
            "$ref": "#/$defs/Alias",
            "description": "annotation"
        });
        assert!(tembra_contracts::normalize_schema(&source).is_err());
        assert_eq!(source["$defs"]["F"], json!(terminal));
    }
    let source = json!({
        "$defs": {"Choices": {"anyOf": [{"type": "string"}, {"type": "integer"}]}},
        "$ref": "#/$defs/Choices/anyOf/0"
    });
    assert_eq!(
        tembra_contracts::normalize_schema(&source).unwrap(),
        json!({"type":"string"})
    );
    let mut invalid_index = source;
    invalid_index["$ref"] = json!("#/$defs/Choices/anyOf/00");
    assert!(tembra_contracts::normalize_schema(&invalid_index).is_err());
}
