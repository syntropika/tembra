//! One WIT value corpus checked through every contract representation.
use super::*;
use serde_json::{Value, json};

pub(crate) const WIT: &str = r#"
package test:conformance@0.1.0;
interface shapes {
    type count = u32;
    type maybe-count = option<count>;
    record point { x-axis: s32, y-axis: s32 }
    flags access { read-all, write-once }
    enum color { red, dark-green }
    variant shape { none, circle(u32), named(string) }
    integers: func(a: u8, b: u16, c: u32, d: s8, e: s16, f: s32) -> count;
    floats: func(single: f32, double: f64) -> f64;
    text: func(letter: char, words: string, flag: bool) -> string;
    point-of: func(value: point) -> point;
    collections: func(values: list<u8>, pair: tuple<string, s32>, maybe: maybe-count) -> list<point>;
    tagged: func(value: shape, colors: list<color>, permissions: access) -> result<u32, string>;
    empty-result: func() -> result;
    nothing: func();
}
interface remote {
    record item { item-name: string }
    fetch: func(key: string) -> option<item>;
    store: func(entry: item) -> result<_, string>;
}
world run { import remote; export shapes; }
"#;

pub(crate) const EXPORT: &str = "test:conformance/shapes@0.1.0";
pub(crate) const IMPORT: &str = "test:conformance/remote@0.1.0";

/// (exports, method, arguments-or-result, value, accepted)
pub(crate) fn corpus() -> Vec<(bool, &'static str, bool, Value, bool)> {
    vec![
        (
            true,
            "integers",
            true,
            json!([255, 65535, 4294967295_u64, -128, -32768, -2147483648_i64]),
            true,
        ),
        (true, "integers", true, json!([256, 0, 0, 0, 0, 0]), false),
        (true, "integers", true, json!([0, 65536, 0, 0, 0, 0]), false),
        (
            true,
            "integers",
            true,
            json!([0, 0, 4294967296_u64, 0, 0, 0]),
            false,
        ),
        (true, "integers", true, json!([0, 0, 0, -129, 0, 0]), false),
        (true, "integers", true, json!([0, 0, 0, 0, 32768, 0]), false),
        (
            true,
            "integers",
            true,
            json!([0, 0, 0, 0, 0, 2147483648_u64]),
            false,
        ),
        (true, "integers", true, json!([-1, 0, 0, 0, 0, 0]), false),
        (true, "integers", true, json!([1.5, 0, 0, 0, 0, 0]), false),
        (true, "integers", true, json!([0, 0, 0, 0, 0]), false),
        (true, "integers", true, json!({"a": 0}), false),
        (true, "integers", false, json!(7), true),
        (true, "integers", false, json!("7"), false),
        (true, "floats", true, json!([1.5, 2.5]), true),
        (true, "floats", true, json!([1e39, 0.0]), false),
        (true, "floats", true, json!([0.0, -0.0]), false),
        (true, "floats", true, json!([3, 4]), true),
        (true, "floats", false, json!(1e300), true),
        (true, "text", true, json!(["x", "words", true]), true),
        (true, "text", true, json!(["é", "", false]), true),
        (true, "text", true, json!(["xy", "words", true]), false),
        (true, "text", true, json!(["", "words", true]), false),
        (true, "text", true, json!(["x", 1, true]), false),
        (true, "text", true, json!(["x", "words", 1]), false),
        (
            true,
            "point-of",
            true,
            json!([{"xAxis": 1, "yAxis": 2}]),
            true,
        ),
        (
            true,
            "point-of",
            true,
            json!([{"x-axis": 1, "y-axis": 2}]),
            false,
        ),
        (true, "point-of", true, json!([{"xAxis": 1}]), false),
        (
            true,
            "point-of",
            true,
            json!([{"xAxis": 1, "yAxis": 2, "z": 3}]),
            false,
        ),
        (true, "point-of", true, json!([[1, 2]]), false),
        (
            true,
            "point-of",
            false,
            json!({"xAxis": -1, "yAxis": 2}),
            true,
        ),
        (
            true,
            "collections",
            true,
            json!([[1, 2, 255], ["a", -1], 4]),
            true,
        ),
        (true, "collections", true, json!([[], ["a", 1], null]), true),
        (
            true,
            "collections",
            true,
            json!([[256], ["a", 1], null]),
            false,
        ),
        (true, "collections", true, json!([[1], ["a"], null]), false),
        (
            true,
            "collections",
            true,
            json!([[1], ["a", 1, 2], null]),
            false,
        ),
        (true, "collections", true, json!([[1], ["a", 1], -1]), false),
        (
            true,
            "collections",
            true,
            json!(["ab", ["a", 1], null]),
            false,
        ),
        (
            true,
            "collections",
            false,
            json!([{"xAxis": 1, "yAxis": 2}]),
            true,
        ),
        (true, "collections", false, json!([{"xAxis": 1}]), false),
        (
            true,
            "tagged",
            true,
            json!([{"tag": "none"}, ["red", "dark-green"], {"readAll": true}]),
            true,
        ),
        (
            true,
            "tagged",
            true,
            json!([{"tag": "circle", "val": 3}, [], {}]),
            true,
        ),
        (
            true,
            "tagged",
            true,
            json!([{"tag": "named", "val": "n"}, [], {"readAll": false, "writeOnce": true}]),
            true,
        ),
        (
            true,
            "tagged",
            true,
            json!([{"tag": "none", "val": 1}, [], {}]),
            false,
        ),
        (
            true,
            "tagged",
            true,
            json!([{"tag": "circle"}, [], {}]),
            false,
        ),
        (
            true,
            "tagged",
            true,
            json!([{"tag": "circle", "val": "x"}, [], {}]),
            false,
        ),
        (
            true,
            "tagged",
            true,
            json!([{"tag": "square"}, [], {}]),
            false,
        ),
        (true, "tagged", true, json!([{"val": 1}, [], {}]), false),
        (
            true,
            "tagged",
            true,
            json!([{"tag": "none"}, ["blue"], {}]),
            false,
        ),
        (
            true,
            "tagged",
            true,
            json!([{"tag": "none"}, ["darkGreen"], {}]),
            false,
        ),
        (
            true,
            "tagged",
            true,
            json!([{"tag": "none"}, [], {"read-all": true}]),
            false,
        ),
        (
            true,
            "tagged",
            true,
            json!([{"tag": "none"}, [], {"readAll": 1}]),
            false,
        ),
        (
            true,
            "tagged",
            true,
            json!([{"tag": "none"}, [], []]),
            false,
        ),
        (true, "tagged", false, json!({"tag": "ok", "val": 1}), true),
        (
            true,
            "tagged",
            false,
            json!({"tag": "err", "val": "bad"}),
            true,
        ),
        (
            true,
            "tagged",
            false,
            json!({"tag": "err", "val": 1}),
            false,
        ),
        (true, "tagged", false, json!({"tag": "ok"}), false),
        (
            true,
            "tagged",
            false,
            json!({"tag": "other", "val": 1}),
            false,
        ),
        (
            true,
            "tagged",
            false,
            json!({"tag": "ok", "val": 1, "extra": 2}),
            false,
        ),
        (true, "empty-result", true, json!([]), true),
        (true, "empty-result", false, json!({"tag": "ok"}), true),
        (true, "empty-result", false, json!({"tag": "err"}), true),
        (
            true,
            "empty-result",
            false,
            json!({"tag": "ok", "val": null}),
            false,
        ),
        (true, "nothing", true, json!([]), true),
        (true, "nothing", true, json!([1]), false),
        (true, "nothing", false, json!(null), true),
        (true, "nothing", false, json!(0), false),
        (true, "missing", true, json!([]), false),
        (false, "fetch", true, json!(["key"]), true),
        (false, "fetch", true, json!([1]), false),
        (false, "fetch", false, json!(null), true),
        (false, "fetch", false, json!({"itemName": "x"}), true),
        (false, "fetch", false, json!({"item-name": "x"}), false),
        (false, "store", true, json!([{"itemName": "x"}]), true),
        (false, "store", false, json!({"tag": "ok"}), true),
        (
            false,
            "store",
            false,
            json!({"tag": "err", "val": "no"}),
            true,
        ),
        (false, "store", false, json!({"tag": "err"}), false),
        (false, "shapes", true, json!([]), false),
    ]
}

/// Bind one corpus entry by its original WIT name, then validate it through the slot.
pub(crate) fn check(
    contract: &PreparedContract,
    (exports, method, arguments, value, accepted): &(bool, &str, bool, Value, bool),
) {
    let result = contract
        .bind(if *exports { EXPORT } else { IMPORT }, method, *exports)
        .and_then(|slot| contract.validate_slot(slot, *arguments, value));
    assert_eq!(
        result.is_ok(),
        *accepted,
        "{method} {} {value}: {result:?}",
        if *arguments { "arguments" } else { "result" }
    );
}

#[test]
fn prepared_runtime_contracts_preserve_wit_value_semantics() {
    let authoring = Contract::from_source("conformance.wit", WIT, None).unwrap();
    authoring.check_value_profile().unwrap();
    authoring.check_import_value_profile(IMPORT).unwrap();
    let artifact = Artifact::decode(
        &authoring
            .artifact(Some("globalThis.__tembraOrb={}"), None, "conformance", 1)
            .unwrap(),
    )
    .unwrap();
    let reparsed = Contract::from_bytes(&authoring.encode().unwrap(), Some("run")).unwrap();
    for entry in corpus() {
        check(&authoring, &entry);
        check(&artifact.contract, &entry);
        check(&reparsed, &entry);
    }
    assert_eq!(artifact.contract.describe(), authoring.describe());
    assert_eq!(artifact.contract, *authoring.prepared());
}

#[test]
fn decoded_contracts_validate_the_described_member_spellings() {
    let source = "package test:spelling;
        interface values {
            flags access { HTTP-ok, x2y }
            record entry { get-HTTP: u32, mode: access }
            put: func(value: entry);
        }
        world run { export values; }";
    let authoring = Contract::from_source("spelling.wit", source, None).unwrap();
    let decoded = PreparedContract::decode(&authoring.prepared().encode()).unwrap();
    let names = &decoded.describe()["javascriptNames"];
    assert_eq!(names["get-HTTP"], "getHttp");
    assert_eq!(names["HTTP-ok"], "httpOk");
    assert_eq!(names["x2y"], "x2y");
    // Slots are positions in the contract, so every representation binds the same index.
    let slot = decoded.bind("test:spelling/values", "put", true).unwrap();
    assert_eq!(
        authoring.bind("test:spelling/values", "put", true).unwrap(),
        slot
    );
    let put = |entry: Value| decoded.validate_slot(slot, true, &json!([entry])).is_ok();
    assert!(put(
        json!({"getHttp": 1, "mode": {"httpOk": true, "x2y": false}})
    ));
    // Only the described spelling is accepted. WIT and other casings are refused.
    for field in ["get-HTTP", "getHTTP", "gethttp"] {
        assert!(!put(json!({field: 1, "mode": {}})), "{field}");
    }
    for flag in ["HTTP-ok", "HTTPOk", "httpok"] {
        assert!(!put(json!({"getHttp": 1, "mode": {flag: true}})), "{flag}");
    }
}

#[test]
fn unknown_methods_and_slots_name_the_requested_side() {
    let contract = Contract::from_source("conformance.wit", WIT, None).unwrap();
    let export = contract.bind(EXPORT, "missing", true).unwrap_err();
    assert!(export.0.contains("unknown WIT export"), "{export}");
    let import = contract.bind(IMPORT, "missing", false).unwrap_err();
    assert!(import.0.contains("unknown WIT import"), "{import}");
    let slots = contract.prepared().slots().len() as u32;
    assert!(contract.validate_slot(slots, true, &json!([])).is_err());
}
