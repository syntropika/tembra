//! Required literal tags select object normalization without ambiguous branch inference.

use std::collections::BTreeMap;

use serde_json::{Value, json};
use tembra_contracts::{Contract, clean_arguments, normalize_input};

fn schema() -> Value {
    json!({"anyOf":[
        {"type":"object","required":["type"],"additionalProperties":false,"properties":{
            "type":{"type":"string","const":"create"},
            "name":{"type":"string","default":"untitled"}
        }},
        {"type":"object","required":["type","id"],"additionalProperties":false,"properties":{
            "type":{"type":"string","const":"remove"},
            "id":{"type":"integer"}
        }}
    ]})
}

#[test]
fn defaults_and_cleaning_follow_the_required_tag_without_mutating_input() {
    let schema = schema();
    let contract = Contract::local("tagged", 1, schema.clone(), BTreeMap::new()).unwrap();
    let input = json!({"type":"remove","id":3,"name":"not a removal field"});
    let before = input.clone();
    assert_eq!(
        normalize_input(&schema, &input).unwrap(),
        json!({"type":"remove","id":3})
    );
    assert_eq!(
        clean_arguments(&schema, &input).unwrap(),
        json!({"type":"remove","id":3})
    );
    assert_eq!(input, before);
    assert_eq!(
        contract.normalize_input(&json!({"type":"create"})).unwrap(),
        json!({"type":"create","name":"untitled"})
    );
    assert_eq!(
        clean_arguments(&schema, &json!({"type":"create"})).unwrap(),
        json!({"type":"create"})
    );
}

#[test]
fn invalid_tags_and_selected_payload_fail_without_trying_another_branch() {
    let schema = schema();
    for input in [
        json!({}),
        json!({"type":"other"}),
        json!({"type":7}),
        json!({"type":"remove"}),
        json!({"type":"remove","id":"wrong"}),
    ] {
        let before = input.clone();
        assert!(
            normalize_input(&schema, &input).is_err(),
            "accepted {input}"
        );
        assert_eq!(input, before);
    }
}

#[test]
fn optional_defaulted_or_overlapping_tags_remain_ambiguous() {
    let mut absent = schema();
    absent["anyOf"][1]["required"] = json!(["id"]);
    let mut duplicate = schema();
    duplicate["anyOf"][1]["properties"]["type"]["const"] = json!("create");
    let mut defaulted = schema();
    defaulted["anyOf"][0]["properties"]["type"]["default"] = json!("create");
    for schema in [absent, duplicate, defaulted] {
        assert_eq!(
            Contract::local("ambiguous", 1, schema, BTreeMap::new())
                .unwrap_err()
                .reason,
            "ambiguous_normalization"
        );
    }
}

#[test]
fn nullable_and_array_wrappers_select_each_nested_tag() {
    let schema = json!({"type":"object","properties":{
        "steps":{"type":"array","items":{"anyOf":[schema(),{"type":"null"}]}},
        "next":{"anyOf":[{"type":"null"},schema()]}
    }});
    let input =
        json!({"steps":[null,{"type":"remove","id":2,"extra":1},{"type":"create"}],"next":null});
    assert_eq!(
        normalize_input(&schema, &input).unwrap(),
        json!({"steps":[null,{"type":"remove","id":2},{"type":"create","name":"untitled"}],"next":null})
    );
}

#[test]
fn tag_survives_identity_and_branch_order_does_not_redirect_normalization() {
    let schema = schema();
    let mut reversed = schema.clone();
    reversed["anyOf"].as_array_mut().unwrap().reverse();
    for input in [json!({"type":"create"}), json!({"type":"remove","id":9})] {
        assert_eq!(
            normalize_input(&schema, &input).unwrap(),
            normalize_input(&reversed, &input).unwrap()
        );
    }
    let a = Contract::local("identity", 1, schema, BTreeMap::new()).unwrap();
    let b = Contract::local("identity", 1, reversed, BTreeMap::new()).unwrap();
    let input = json!({"type":"remove","id":9});
    assert_eq!(a.identity(&input).unwrap(), b.identity(&input).unwrap());
}
