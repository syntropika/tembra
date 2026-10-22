use std::{collections::BTreeMap, sync::Arc};

use serde_json::{Value, json};
use tembra_contracts::{
    CompileLimits, Contract, Member, Placement, PreparedSchema, ValueSchema, clean_arguments,
    normalize_input, normalize_schema, normalize_schema_with_limits, validate,
};

#[test]
fn prepared_operations_preserve_values_and_exact_raw_errors() {
    let schemas = [
        json!(true),
        json!(false),
        json!({"type":"integer", "minimum":-1, "maximum":2}),
        json!({"type":"number", "exclusiveMinimum":-1, "exclusiveMaximum":2}),
        json!({"type":"string", "minLength":1, "maxLength":2}),
        json!({"type":"array", "items":{"type":"integer"}, "minItems":1,
            "maxItems":2, "uniqueItems":true}),
        json!({"type":"object", "properties":{"count":{"type":"integer", "default":0}},
            "required":["count"], "additionalProperties":false}),
        json!({"const":{"count":1}}),
        json!({"enum":[null, "ok", 1]}),
        json!({"anyOf":[{"type":"integer"}, {"type":"string", "minLength":2}]}),
        json!({"type":["null", "string"], "maxLength":2}),
        json!({"$defs":{"item":{"type":"integer", "minimum":0}},
            "type":"array", "items":{"$ref":"#/$defs/item"}}),
        json!({"anyOf":[
            {"type":"object", "properties":{"kind":{"const":"a"}, "a":{"type":"integer"}},
                "required":["kind","a"]},
            {"type":"object", "properties":{"kind":{"const":"b"}, "b":{"type":"string"}},
                "required":["kind","b"]}
        ]}),
    ];
    let mut values = vec![
        Value::Null,
        json!(false),
        json!(true),
        json!(-2),
        json!(-1),
        json!(0),
        json!(1),
        json!(1.5),
        json!(2),
        json!(3),
        json!(""),
        json!("a"),
        json!("ok"),
        json!("abc"),
        json!([]),
        json!([1]),
        json!([1, 2]),
        json!([1, 1]),
        json!([1, "bad"]),
        json!({}),
        json!({"count":1}),
        json!({"count":"bad"}),
        json!({"count":1,"extra":true}),
        json!({"kind":"a","a":1,"b":"discarded"}),
        json!({"kind":"b","b":"ok"}),
        json!(9_007_199_254_740_992_u64),
    ];
    let mut deep = Value::Null;
    for _ in 0..130 {
        deep = json!([deep]);
    }
    values.push(deep);
    for source in schemas {
        let source_before = source.clone();
        let prepared = PreparedSchema::new(&source).unwrap();
        let normalized = normalize_schema(&source).unwrap();
        assert_eq!(prepared.normalized(), &normalized);
        for value in &values {
            let before = value.clone();
            assert_eq!(prepared.validate(value), validate(&normalized, value));
            assert_eq!(
                prepared.clean_arguments(value),
                clean_arguments(&normalized, value)
            );
            assert_eq!(
                prepared.normalize_input(value),
                normalize_input(&normalized, value)
            );
            assert_eq!(value, &before);
        }
        assert_eq!(source, source_before);
    }
}

#[test]
fn validation_only_unions_defer_cleaning_refusal_and_never_memoize_value_errors() {
    let source = json!({"anyOf":[
        {"type":"object","properties":{"a":{"type":"integer"}},"required":["a"]},
        {"type":"object","properties":{"b":{"type":"string"}},"required":["b"]}
    ]});
    let prepared = PreparedSchema::new(&source).unwrap();
    prepared.validate(&json!({"a":1})).unwrap();
    let error = prepared.clean_arguments(&json!({"a":1})).unwrap_err();
    assert_eq!(error.reason, "ambiguous_normalization");
    assert_eq!(
        error,
        clean_arguments(&source, &json!({"a":1})).unwrap_err()
    );
    let unsafe_value = json!(9_007_199_254_740_992_u64);
    assert_eq!(
        prepared.clean_arguments(&unsafe_value),
        clean_arguments(&source, &unsafe_value)
    );
    assert_eq!(
        prepared.normalize_input(&unsafe_value),
        normalize_input(&source, &unsafe_value)
    );
    assert_eq!(
        prepared.clean_arguments(&unsafe_value).unwrap_err().reason,
        "number"
    );
    let copy = prepared.clone();
    assert!(copy.validate(&json!({"a":"bad"})).is_err());
    prepared.validate(&json!({"b":"ok"})).unwrap();
}

#[test]
fn checked_preparation_preserves_limits_and_owns_its_source() {
    for source in [
        json!(0),
        json!({"type":"unknown"}),
        json!({"unsupported":true}),
        json!({"$ref":"#/$defs/missing"}),
        json!({"$defs":{"loop":{"$ref":"#/$defs/loop"}},"$ref":"#/$defs/loop"}),
        json!({"type":"integer","default":"bad"}),
    ] {
        assert_eq!(
            PreparedSchema::new(&source).err(),
            normalize_schema(&source).err()
        );
    }
    let mut source = json!({"type":"object","properties":{"n":{"type":"integer"}}});
    let limits = CompileLimits {
        max_depth: 0,
        max_nodes: 1,
    };
    assert_eq!(
        PreparedSchema::with_limits(&source, limits).err(),
        normalize_schema_with_limits(&source, limits).err()
    );
    let prepared = PreparedSchema::new(&source).unwrap();
    source["properties"]["n"]["type"] = json!("string");
    drop(source);
    prepared.validate(&json!({"n":1})).unwrap();
    assert!(prepared.validate(&json!({"n":"bad"})).is_err());
}

#[test]
fn contract_metadata_slots_wire_equality_and_thread_traits_remain_stable() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<Contract>();
    send_sync::<PreparedSchema>();
    let method = |args, result| Member::Method { args, result };
    let contract = Contract::remote(
        "prepared.slots",
        1,
        json!({}),
        Placement::Named {
            name: "server".into(),
        },
        BTreeMap::from([
            (
                "one".into(),
                BTreeMap::from([
                    (
                        "method".into(),
                        method(
                            Some(ValueSchema::Portable(json!({"type":"integer"}))),
                            Some(ValueSchema::Portable(json!({"type":"string"}))),
                        ),
                    ),
                    ("empty".into(), method(None, None)),
                    (
                        "state".into(),
                        Member::State {
                            schema: json!({"type":"boolean"}),
                        },
                    ),
                ]),
            ),
            (
                "two".into(),
                BTreeMap::from([(
                    "method".into(),
                    method(Some(ValueSchema::Portable(json!({"type":"string"}))), None),
                )]),
            ),
        ]),
    )
    .unwrap();
    let wire = serde_json::to_value(&contract).unwrap();
    let debug = format!("{contract:?}");
    let fingerprint = contract.fingerprint().to_owned();
    assert_eq!(wire, serde_json::to_value(contract.descriptor()).unwrap());
    contract
        .validate_method_args("one", "method", &json!(1))
        .unwrap();
    assert!(
        contract
            .validate_method_args("one", "method", &json!("bad"))
            .is_err()
    );
    contract
        .validate_method_args("two", "method", &json!("ok"))
        .unwrap();
    contract
        .validate_method_result("one", "method", &json!("ok"))
        .unwrap();
    contract
        .validate_state("one", "state", &json!(false))
        .unwrap();
    assert!(
        contract
            .validate_method_args("one", "empty", &Value::Null)
            .is_err()
    );
    assert!(
        contract
            .validate_method_args("unknown", "method", &Value::Null)
            .is_err()
    );
    assert!(
        contract
            .validate_method_args("one", "state", &Value::Null)
            .is_err()
    );
    let _ = contract.normalize_input(&json!({"extra":1})).unwrap();
    let decoded = Contract::from_json(&serde_json::to_string(&contract).unwrap()).unwrap();
    assert_eq!(decoded, contract);
    assert_eq!(format!("{contract:?}"), debug);
    assert_eq!(contract.fingerprint(), fingerprint);
    assert_eq!(serde_json::to_value(&contract).unwrap(), wire);
    let native = Contract::local(
        "prepared.native",
        1,
        json!({}),
        BTreeMap::from([(
            "method".into(),
            method(Some(ValueSchema::Native("value@1".into())), None),
        )]),
    )
    .unwrap();
    assert!(
        native
            .validate_method_args("", "method", &Value::Null)
            .is_err()
    );
    let shared = Arc::new(contract);
    std::thread::spawn(move || shared.normalize_input(&Value::Null).unwrap())
        .join()
        .unwrap();
}

#[test]
fn descriptor_retains_second_pass_budget_errors_until_actual_value_use() {
    let properties = (0..9_000)
        .map(|index| (format!("a{index}"), json!({"type":"array"})))
        .collect::<serde_json::Map<_, _>>();
    let source = json!({"type":"object", "properties":properties});
    let direct = PreparedSchema::new(&source).unwrap();
    assert_eq!(direct.validate(&json!({})), validate(&source, &json!({})));
    let normalized = normalize_schema(&source).unwrap();
    let expected = validate(&normalized, &json!({})).unwrap_err();
    assert_eq!(expected.reason, "schema_budget");
    let contract = Contract::local(
        "prepared.budget",
        1,
        json!({}),
        BTreeMap::from([
            (
                "call".into(),
                Member::Method {
                    args: Some(ValueSchema::Portable(source.clone())),
                    result: Some(ValueSchema::Portable(source.clone())),
                },
            ),
            ("state".into(), Member::State { schema: source }),
        ]),
    )
    .unwrap();
    for value in [json!({}), json!(9_007_199_254_740_992_u64)] {
        assert_eq!(
            contract.validate_method_args("", "call", &value),
            validate(&normalized, &value)
        );
        assert_eq!(
            contract.clean_method_args("", "call", &value),
            clean_arguments(&normalized, &value)
        );
        assert_eq!(
            contract.validate_method_result("", "call", &value),
            validate(&normalized, &value)
        );
        assert_eq!(
            contract.validate_state("", "state", &value),
            validate(&normalized, &value)
        );
    }
    assert_eq!(
        contract
            .validate_method_args("", "call", &json!({}))
            .unwrap_err(),
        expected
    );
}

#[test]
fn method_profile_keys_follow_actual_contract_members_and_cloned_owners() {
    use tembra_contracts::CanonicalProfileKey;
    let input = Some(ValueSchema::Portable(json!({"type":"integer","minimum":1})));
    let equivalent = Some(ValueSchema::Portable(
        json!({"minimum":1.0,"type":"integer"}),
    ));
    let output = Some(ValueSchema::Native("answer@1".into()));
    let token = Contract::local(
        "profiles",
        1,
        json!({}),
        BTreeMap::from([
            (
                "run".into(),
                Member::Method {
                    args: input.clone(),
                    result: output.clone(),
                },
            ),
            (
                "empty".into(),
                Member::Method {
                    args: None,
                    result: None,
                },
            ),
            (
                "state".into(),
                Member::State {
                    schema: json!({"type":"integer"}),
                },
            ),
        ]),
    )
    .unwrap();
    let mut detached = token.descriptor().clone();
    detached.variants.get_mut("").unwrap().insert(
        "run".into(),
        Member::Method {
            args: None,
            result: None,
        },
    );
    let changed = Contract::from_descriptor({
        detached.fingerprint.clear();
        detached
    })
    .unwrap();
    let copy = token.clone();
    drop(token);
    let a = CanonicalProfileKey::new(&equivalent).unwrap();
    let b = CanonicalProfileKey::new(&output).unwrap();
    let absent = CanonicalProfileKey::new(&None).unwrap();
    assert!(copy.method_profiles_match("", "run", &a, &b));
    assert!(!changed.method_profiles_match("", "run", &a, &b));
    assert!(!copy.method_profiles_match("missing", "run", &a, &b));
    assert!(!copy.method_profiles_match("", "missing", &a, &b));
    assert!(!copy.method_profiles_match("", "state", &a, &b));
    assert!(!copy.method_profiles_match("", "run", &b, &a));
    assert!(copy.method_profiles_match("", "empty", &absent, &absent));
    assert!(!copy.method_profiles_match("", "empty", &a, &absent));
    assert_ne!(
        absent,
        CanonicalProfileKey::new(&Some(ValueSchema::Portable(json!(null)))).unwrap()
    );
    assert_ne!(
        b,
        CanonicalProfileKey::new(&Some(ValueSchema::Portable(json!({"type":"string"})))).unwrap()
    );
    assert_ne!(
        CanonicalProfileKey::new(&Some(ValueSchema::Native("x".into()))).unwrap(),
        CanonicalProfileKey::new(&Some(ValueSchema::Portable(json!({"native":"x"})))).unwrap()
    );
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Contract>();
    assert_send_sync::<CanonicalProfileKey>();
    let invalid = Some(ValueSchema::Portable(
        json!({"minimum":9_007_199_254_740_992_u64}),
    ));
    assert_eq!(
        CanonicalProfileKey::new(&invalid).unwrap_err().reason,
        "number"
    );
}
