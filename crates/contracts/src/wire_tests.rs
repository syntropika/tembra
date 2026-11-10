//! WIT `u64`/`s64` values: exact numbers in native form, canonical decimal strings on the wire.

use super::*;
use serde_json::{Value, json};

const SOURCE: &str = r#"
package example:wide;
interface values {
    record mixed { small: u32, large-count: u64, signed: s64, label: string, nested: list<option<s64>> }
    variant pick { none, count(u64), name(string) }
    type counts = list<u64>;
    echo: func(value: mixed, pair: tuple<u64, s32>, choice: pick) -> result<counts, s64>;
    extreme: func(a: u64, b: s64) -> s64;
    narrow: func(a: u32, b: string) -> list<s32>;
}
world run { export values; }
"#;

fn contract() -> Contract {
    Contract::from_source("wide.wit", SOURCE, None).unwrap()
}

fn slot(contract: &Contract, name: &str) -> u32 {
    contract.bind("example:wide/values", name, true).unwrap()
}

/// Integer JSON literals within u64/i64 are exact with and without `arbitrary_precision`.
fn exact(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

#[test]
fn extreme_values_round_trip_exactly() {
    let contract = contract();
    let extreme = slot(&contract, "extreme");
    for (native, wire) in [
        (
            exact("[18446744073709551615, -9223372036854775808]"),
            json!(["18446744073709551615", "-9223372036854775808"]),
        ),
        (
            exact("[0, 9223372036854775807]"),
            json!(["0", "9223372036854775807"]),
        ),
        (
            exact("[9007199254740993, -1]"),
            json!(["9007199254740993", "-1"]),
        ),
    ] {
        assert!(contract.validate_slot(extreme, true, &native).is_ok());
        assert!(contract.validate_wire(extreme, true, &wire).is_ok());
        let converted = contract.to_wire(extreme, true, native.clone()).unwrap();
        assert_eq!(converted, wire);
        assert_eq!(contract.from_wire(extreme, true, wire).unwrap(), native);
    }
    let result = exact("-9223372036854775808");
    let wire = contract.to_wire(extreme, false, result.clone()).unwrap();
    assert_eq!(wire, json!("-9223372036854775808"));
    assert_eq!(contract.from_wire(extreme, false, wire).unwrap(), result);
    // Typed Rust values reach the same exact numbers without any wrapper.
    let typed = serde_json::to_value((u64::MAX, i64::MIN)).unwrap();
    assert_eq!(typed, exact("[18446744073709551615, -9223372036854775808]"));
    let back: (u64, i64) = serde_json::from_value(
        contract
            .from_wire(
                extreme,
                true,
                contract.to_wire(extreme, true, typed).unwrap(),
            )
            .unwrap(),
    )
    .unwrap();
    assert_eq!(back, (u64::MAX, i64::MIN));
}

#[test]
fn malformed_and_out_of_range_spellings_are_refused() {
    let contract = contract();
    let extreme = slot(&contract, "extreme");
    let wire = |a: Value, b: Value| contract.from_wire(extreme, true, json!([a, b]));
    assert!(wire(json!("1"), json!("-1")).is_ok());
    for bad in [
        "",
        "01",
        "00",
        "+1",
        "-0",
        " 1",
        "1 ",
        "1e3",
        "1.0",
        "0x1",
        "-",
        "1_000",
        "18446744073709551616",
        "-1",
    ] {
        assert!(wire(json!(bad), json!("0")).is_err(), "u64 wire {bad:?}");
    }
    for bad in [
        "-0",
        "-01",
        "+1",
        "9223372036854775808",
        "-9223372036854775809",
        "1e3",
        "--1",
    ] {
        assert!(wire(json!("0"), json!(bad)).is_err(), "s64 wire {bad:?}");
    }
    // The wire never carries a number for a 64-bit position, even a small exact one.
    assert!(wire(json!(1), json!("0")).is_err());
    assert!(wire(json!("0"), json!(-1)).is_err());
    assert!(wire(Value::Null, json!("0")).is_err());

    let native = |text: &str| contract.to_wire(extreme, true, exact(text));
    assert!(native("[1, -1]").is_ok());
    for bad in [
        r#"["1", 0]"#,
        r#"[1.0, 0]"#,
        r#"[1e3, 0]"#,
        r#"[-1, 0]"#,
        r#"[18446744073709551616, 0]"#,
        r#"[0, 9223372036854775808]"#,
        r#"[0, -9223372036854775809]"#,
        r#"[0, "0"]"#,
        r#"[0]"#,
        r#"[0, 0, 0]"#,
    ] {
        assert!(native(bad).is_err(), "native {bad}");
    }
    assert!(contract.to_wire(extreme, false, json!("0")).is_err());
    assert!(contract.from_wire(extreme, false, json!(0)).is_err());
}

#[test]
fn mixed_nested_forms_change_only_64_bit_leaves() {
    let contract = contract();
    let echo = slot(&contract, "echo");
    let native = exact(
        r#"[
            {"small": 4294967295, "largeCount": 18446744073709551615, "signed": -5,
             "label": "123", "nested": [null, 9223372036854775807, -9223372036854775808]},
            [18446744073709551614, -7],
            {"tag": "count", "val": 9007199254740993}
        ]"#,
    );
    let wire = json!([
        {"small": 4294967295_u32, "largeCount": "18446744073709551615", "signed": "-5",
         "label": "123", "nested": [null, "9223372036854775807", "-9223372036854775808"]},
        ["18446744073709551614", -7],
        {"tag": "count", "val": "9007199254740993"}
    ]);
    assert_eq!(contract.to_wire(echo, true, native.clone()).unwrap(), wire);
    assert_eq!(
        contract.from_wire(echo, true, wire.clone()).unwrap(),
        native
    );

    // A WIT string keeps its digits; a 32-bit position stays a number in both forms.
    let mut text = wire.clone();
    text[0]["label"] = json!(123);
    assert!(contract.from_wire(echo, true, text).is_err());
    let mut small = wire.clone();
    small[0]["small"] = json!("4294967295");
    assert!(contract.from_wire(echo, true, small).is_err());
    let mut pair = wire.clone();
    pair[1][1] = json!("-7");
    assert!(contract.from_wire(echo, true, pair).is_err());

    // Cases without 64-bit payloads are untouched.
    let mut name = native.clone();
    name[2] = json!({"tag": "name", "val": "18446744073709551615"});
    let mut named = contract.to_wire(echo, true, name.clone()).unwrap();
    assert_eq!(named[2], name[2]);
    named[2] = json!({"tag": "none"});
    assert_eq!(
        contract.from_wire(echo, true, named).unwrap()[2],
        json!({"tag": "none"})
    );

    // Results respell through an alias of a list, and through the error payload.
    for (native, wire) in [
        (
            exact(r#"{"tag":"ok","val":[0, 18446744073709551615]}"#),
            json!({"tag":"ok","val":["0","18446744073709551615"]}),
        ),
        (
            exact(r#"{"tag":"err","val":-9223372036854775808}"#),
            json!({"tag":"err","val":"-9223372036854775808"}),
        ),
    ] {
        assert_eq!(contract.to_wire(echo, false, native.clone()).unwrap(), wire);
        assert_eq!(contract.from_wire(echo, false, wire).unwrap(), native);
    }
    assert!(
        contract
            .from_wire(echo, false, json!({"tag":"ok","val":[1]}))
            .is_err()
    );
}

#[test]
fn signatures_without_64_bit_integers_pass_through_and_packets_are_strict() {
    let contract = contract();
    let narrow = slot(&contract, "narrow");
    let arguments = json!([7, "18446744073709551615"]);
    assert_eq!(
        contract.to_wire(narrow, true, arguments.clone()).unwrap(),
        arguments
    );
    assert_eq!(
        contract.from_wire(narrow, true, arguments.clone()).unwrap(),
        arguments
    );
    assert!(contract.to_wire(narrow, true, json!(["7", "x"])).is_err());
    assert_eq!(
        contract.from_wire(narrow, false, json!([-1])).unwrap(),
        json!([-1])
    );

    // Executor packets are the wire form: strings only, checked by the same validator.
    let extreme = slot(&contract, "extreme");
    let accepted = |text: &str| {
        let response: Value =
            serde_json::from_str(&contract.validate_packet(extreme, true, text)).unwrap();
        response.get("ok").is_some()
    };
    assert!(accepted(
        r#"["18446744073709551615","-9223372036854775808"]"#
    ));
    assert!(!accepted(r#"[18446744073709551615,-9223372036854775808]"#));
    assert!(!accepted(r#"[1,"0"]"#));
    assert!(!accepted(r#"["018446744073709551615","0"]"#));
    assert!(!accepted(r#"["18446744073709551616","0"]"#));
    assert!(!accepted(r#"["0","-0"]"#));
}
