use std::collections::{BTreeMap, BTreeSet};

use schemars::JsonSchema;
use serde_json::Value;
use tembra_contracts::{Contract, canonical_json, normalize_schema, schema_for};

fn corpus() -> Value {
    serde_json::from_str(include_str!("../../../fixtures/contracts/review-v1.json")).unwrap()
}

#[test]
fn negative_schema_vectors_pin_stage_path_and_reason() {
    for case in corpus()["schemaRejections"].as_array().unwrap() {
        assert_eq!(case["stage"], "schema");
        let error = normalize_schema(&case["schema"]).unwrap_err();
        assert_eq!(error.reason, case["reason"], "{}", case["name"]);
        assert_eq!(error.path, case["path"], "{}", case["name"]);
    }
}

#[test]
fn canonical_digest_vectors_cover_each_descriptor_change() {
    let corpus = corpus();
    let mut digests = BTreeSet::new();
    for case in corpus["digests"].as_array().unwrap() {
        let contract = Contract::from_json(&case["descriptor"].to_string()).unwrap();
        assert_eq!(
            contract.fingerprint(),
            case["fingerprint"].as_str().unwrap()
        );
        let mut descriptor = serde_json::to_value(&contract).unwrap();
        descriptor.as_object_mut().unwrap().remove("fingerprint");
        assert_eq!(canonical_json(&descriptor).unwrap(), case["canonical"]);
        assert!(digests.insert(contract.fingerprint().to_owned()));
        let mut forged = case["descriptor"].clone();
        forged["fingerprint"] = Value::String("0".repeat(64));
        assert_eq!(
            Contract::from_json(&forged.to_string()).unwrap_err().reason,
            "fingerprint"
        );
    }
    for case in corpus["canonical"].as_array().unwrap() {
        assert_eq!(canonical_json(&case["value"]).unwrap(), case["bytes"]);
    }
}

#[derive(JsonSchema)]
#[schemars(title = "ReferenceItem")]
struct ReferenceItem {
    #[expect(
        dead_code,
        reason = "only the type's independently derived schema is exercised"
    )]
    label: String,
}

#[derive(JsonSchema)]
#[schemars(title = "ReferenceInput")]
struct ReferenceInput {
    #[expect(
        dead_code,
        reason = "only the type's independently derived schema is exercised"
    )]
    left: ReferenceItem,
    #[expect(
        dead_code,
        reason = "only the type's independently derived schema is exercised"
    )]
    right: ReferenceItem,
}

#[test]
fn independently_authored_repeated_definitions_match_the_shared_descriptor() {
    let corpus = corpus();
    let case = &corpus["references"];
    let source = schema_for::<ReferenceInput>().unwrap();
    let rust = Contract::local("fixture.refs", 1, source, BTreeMap::new()).unwrap();
    let fixture =
        Contract::local("fixture.refs", 1, case["schema"].clone(), BTreeMap::new()).unwrap();
    assert_eq!(rust, fixture);
    assert_eq!(rust.fingerprint(), case["fingerprint"].as_str().unwrap());
    let mut descriptor = serde_json::to_value(rust).unwrap();
    descriptor.as_object_mut().unwrap().remove("fingerprint");
    assert_eq!(canonical_json(&descriptor).unwrap(), case["canonical"]);
}
