use serde::Deserialize;
use serde_json::json;
use tembra_contracts::from_portable_value;

#[derive(Debug, Deserialize, PartialEq)]
struct Clock {
    wake_at_ms: u64,
    nested: Vec<Option<i64>>,
}

#[test]
fn javascript_integral_numbers_decode_exactly_without_mutating_borrowed_input() {
    let value = json!({"wake_at_ms":1_792_536_326_221.0,"nested":[-9_007_199_254_740_991.0,null,9_007_199_254_740_991.0]});
    let before = value.clone();
    assert_eq!(
        from_portable_value::<Clock>(&value).unwrap(),
        Clock {
            wake_at_ms: 1_792_536_326_221,
            nested: vec![
                Some(-9_007_199_254_740_991),
                None,
                Some(9_007_199_254_740_991)
            ],
        }
    );
    assert_eq!(value, before);
    assert!(value["wake_at_ms"].as_number().unwrap().is_f64());
}

#[test]
fn fractions_negative_unsigned_values_and_non_numeric_shapes_are_not_coerced() {
    for value in [
        json!(1.25),
        json!(-1.0),
        json!("42"),
        json!(null),
        json!(true),
    ] {
        let before = value.clone();
        assert_eq!(
            from_portable_value::<u64>(&value).unwrap_err().reason,
            "deserialization"
        );
        assert_eq!(value, before);
    }
    assert_eq!(from_portable_value::<f64>(&json!(1.25)).unwrap(), 1.25);
    assert_eq!(from_portable_value::<i64>(&json!(-17.0)).unwrap(), -17);
    assert_eq!(from_portable_value::<u64>(&json!(-0.0)).unwrap(), 0);
}

#[test]
fn unsafe_numbers_and_excessive_nesting_fail_the_portable_profile() {
    for value in [
        json!(9_007_199_254_740_992_u64),
        json!(-9_007_199_254_740_992_i64),
        json!(u64::MAX),
        json!(1.0e100),
    ] {
        assert_eq!(
            from_portable_value::<serde_json::Value>(&value)
                .unwrap_err()
                .reason,
            "number"
        );
    }
    let mut value = json!(null);
    for _ in 0..129 {
        value = json!([value]);
    }
    assert_eq!(
        from_portable_value::<serde_json::Value>(&value)
            .unwrap_err()
            .reason,
        "depth"
    );
}
