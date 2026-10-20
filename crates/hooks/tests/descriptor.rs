use serde_json::{Value, json};
use tembra_core::contracts::ValueSchema;
use tembra_hooks::*;
#[test]
fn independent_producers_match_all_shared_descriptor_goldens() {
    let fixture: Value =
        serde_json::from_str(include_str!("../../../fixtures/hooks/v1.json")).unwrap();
    for vector in fixture["valid"].as_array().unwrap() {
        let checked = HookDescriptor::from_json(&vector["descriptor"].to_string()).unwrap();
        assert_eq!(checked.canonical(), vector["canonical"].as_str().unwrap());
        assert_eq!(
            checked.registry().name(),
            vector["registry"].as_str().unwrap()
        );
        assert_eq!(
            serde_json::to_value(checked.contribution_member()).unwrap(),
            vector["member"]
        );
        let descriptor = checked.descriptor();
        let authored = HookDescriptor::new(
            descriptor.name.clone(),
            descriptor.version,
            descriptor.strategy,
            descriptor.input.clone(),
            descriptor.result.clone(),
            descriptor.terminal.clone(),
        )
        .unwrap();
        assert_eq!(authored.fingerprint(), checked.fingerprint());
    }
    let native = |id: &str| Some(ValueSchema::Native(id.into()));
    let authored = HookDescriptor::new(
        "example.operation",
        1,
        Strategy::Middleware,
        native("example.input@1"),
        native("example.result@1"),
        Some(TerminalProfile {
            input: native("example.input@1"),
            result: native("example.result@1"),
        }),
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(authored.descriptor()).unwrap(),
        fixture["valid"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["id"] == "middleware-native")
            .unwrap()["descriptor"]
    );
    let bail = HookDescriptor::new(
        "example.choose",
        1,
        Strategy::Bail,
        None,
        Some(ValueSchema::Portable(
            json!({"anyOf":[{"type":"boolean"},{"type":"null"}]}),
        )),
        None,
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(bail.descriptor()).unwrap(),
        fixture["valid"][1]["descriptor"]
    );
    let portable = |value| Some(ValueSchema::Portable(value));
    for (id, authored) in [
        (
            "parallel-portable-terminal",
            HookDescriptor::new(
                "example.parallel",
                1,
                Strategy::Parallel,
                portable(json!({"type":"string"})),
                portable(json!({"type":"integer"})),
                Some(TerminalProfile {
                    input: portable(json!({"type":"array","items":{"type":"integer"}})),
                    result: None,
                }),
            )
            .unwrap(),
        ),
        (
            "parallel-native-terminal",
            HookDescriptor::new(
                "example.native-parallel",
                1,
                Strategy::Parallel,
                native("example.input@1"),
                native("example.item@1"),
                Some(TerminalProfile {
                    input: native("example.items@1"),
                    result: portable(json!({"type":"integer"})),
                }),
            )
            .unwrap(),
        ),
    ] {
        assert_eq!(
            serde_json::to_value(authored.descriptor()).unwrap(),
            fixture["valid"]
                .as_array()
                .unwrap()
                .iter()
                .find(|v| v["id"] == id)
                .unwrap()["descriptor"]
        );
    }
}
#[test]
fn required_presence_and_strategy_terminal_shapes_fail_closed() {
    let good = HookDescriptor::new("unit", 1, Strategy::Series, None, None, None).unwrap();
    let mut value = serde_json::to_value(good.descriptor()).unwrap();
    value.as_object_mut().unwrap().remove("input");
    assert!(HookDescriptor::from_json(&value.to_string()).is_err());
    let mut value = serde_json::to_value(good.descriptor()).unwrap();
    value["fingerprint"] = json!("0".repeat(64));
    assert!(HookDescriptor::from_json(&value.to_string()).is_err());
    assert!(HookDescriptor::new("bad", 1, Strategy::Middleware, None, None, None).is_err());
    assert!(
        HookDescriptor::new(
            "bad",
            1,
            Strategy::Parallel,
            None,
            None,
            Some(TerminalProfile {
                input: None,
                result: None
            })
        )
        .is_err()
    );
    assert!(
        HookDescriptor::new(
            "bad",
            1,
            Strategy::Parallel,
            None,
            Some(ValueSchema::Portable(json!({"type":"integer"}))),
            Some(TerminalProfile {
                input: Some(ValueSchema::Portable(json!({"type":"array"}))),
                result: None
            })
        )
        .is_err()
    );
}
