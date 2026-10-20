//! Descriptor-v1 acquisition behavior from Tembra's documented normalization policy.
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use tembra_contracts::{Contract, ContractError, normalize_input};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    version: u32,
    groups: Vec<Group>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Group {
    name: String,
    schema: Value,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    name: String,
    input: Value,
    expected: Expected,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Expected {
    Accepted(Accepted),
    Refused(Refused),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Accepted {
    accepted: bool,
    normalized: Value,
    identity: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Refused {
    accepted: bool,
}

#[test]
fn acquisition_normalization_identity_and_refusal_preserve_the_callers_input() {
    let fixture: Fixture = serde_json::from_str(include_str!(
        "../../../fixtures/contracts/normalization-v1.json"
    ))
    .unwrap();
    assert_eq!(fixture.version, 1);
    assert!(!fixture.groups.is_empty());
    for group in fixture.groups {
        let contract = Contract::local(
            "regression.identity",
            1,
            group.schema.clone(),
            BTreeMap::new(),
        )
        .unwrap_or_else(|error| panic!("{}: {error}", group.name));
        assert!(
            !group.cases.is_empty(),
            "{} needs behavioral cases",
            group.name
        );
        for case in group.cases {
            let context = format!("{} / {}", group.name, case.name);
            let before = case.input.clone();
            let normalized = normalize_input(&group.schema, &case.input);
            assert_eq!(case.input, before, "{context}: normalization changed input");
            let identity = contract.identity(&case.input);
            assert_eq!(case.input, before, "{context}: identity changed input");
            match case.expected {
                Expected::Accepted(expected) => {
                    assert!(expected.accepted, "{context}: inconsistent expectation");
                    assert_eq!(
                        normalized.unwrap_or_else(|error| panic!("{context}: {error}")),
                        expected.normalized,
                        "{context}"
                    );
                    assert_eq!(
                        identity.unwrap_or_else(|error| panic!("{context}: {error}")),
                        expected.identity,
                        "{context}"
                    );
                }
                Expected::Refused(expected) => {
                    assert!(!expected.accepted, "{context}: incomplete expectation");
                    let normalization_error: ContractError = match normalized {
                        Err(error) => error,
                        Ok(value) => panic!("{context}: normalization accepted {value}"),
                    };
                    let identity_error: ContractError = match identity {
                        Err(error) => error,
                        Ok(value) => panic!("{context}: identity accepted {value}"),
                    };
                    assert_eq!(normalization_error.reason, "validation", "{context}");
                    assert_eq!(identity_error.reason, "validation", "{context}");
                }
            }
        }
    }
}
