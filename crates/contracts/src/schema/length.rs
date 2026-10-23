//! Canonical byte measurement for consumers that do not retain encoded bytes.

use std::io::{self, Write};

use serde::Serialize;
use serde_json::Value;

use crate::{ContractError, Result};

#[derive(Default)]
struct Counter(usize);

impl Counter {
    fn add(&mut self, amount: usize) -> Result<()> {
        self.0 = self.0.checked_add(amount).ok_or_else(|| {
            ContractError::new("canonical", "canonical byte length exceeds usize")
        })?;
        Ok(())
    }

    fn scalar(&mut self, value: &impl Serialize) -> Result<()> {
        serde_jcs::to_writer(self, value)
            .map_err(|error| ContractError::new("canonical", error.to_string()))
    }

    fn value(&mut self, value: &Value) -> Result<()> {
        match value {
            Value::Array(values) => {
                self.add(2 + values.len().saturating_sub(1))?;
                for value in values {
                    self.value(value)?;
                }
            }
            Value::Object(values) => {
                self.add(2 + values.len().saturating_sub(1))?;
                for (key, value) in values {
                    self.scalar(key)?;
                    self.add(1)?;
                    self.value(value)?;
                }
            }
            _ => self.scalar(value)?,
        }
        Ok(())
    }
}

impl Write for Counter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .ok_or_else(|| io::Error::other("canonical byte length exceeds usize"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Measures the exact canonical UTF-8 length without retaining bytes or sorting keys.
///
/// Portability is checked first in the same order as [`super::canonical_json`]. The
/// existing JCS serializer formats each scalar and key; this operation only counts
/// container punctuation. Key ordering cannot change the sum of encoded lengths.
/// The borrowed value is never modified, including on failure.
///
/// # Errors
/// Returns the same portability/depth/number errors as canonical serialization and
/// preserves JCS scalar failures. Returns `canonical` if the count exceeds `usize`.
#[cfg(test)]
fn canonical_json_len(value: &Value) -> Result<usize> {
    super::portable(value, 0)?;
    let mut counter = Counter::default();
    counter.value(value)?;
    Ok(counter.0)
}

// Only owned output crosses this crate-private seam. There is no reusable
// caller-constructible proof that a Value has already been validated.
pub(crate) enum DecodePreflight {
    Bytes(String),
    Length(usize),
}

pub(crate) fn decode_preflight(value: &Value) -> Result<DecodePreflight> {
    let numeric = super::portable_walk::<true>(value, 0)?;
    if numeric {
        serde_jcs::to_string(value)
            .map(DecodePreflight::Bytes)
            .map_err(|error| ContractError::new("canonical", error.to_string()))
    } else {
        let mut counter = Counter::default();
        counter.value(value)?;
        Ok(DecodePreflight::Length(counter.0))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Number, Value, json};

    use super::canonical_json_len;

    fn values() -> Vec<(String, Value)> {
        let mut cases = vec![
            ("null".into(), Value::Null),
            ("boolean".into(), json!(true)),
            ("empty array".into(), json!([])),
            ("empty object".into(), json!({})),
            ("empty string".into(), json!("")),
            (
                "escapes".into(),
                json!("\"\\/\u{0000}\u{0008}\u{000c}\n\r\t\u{001f}"),
            ),
            (
                "Unicode and UTF16 key order".into(),
                json!({"\u{e000}":2,"😀":1,"é":"中"}),
            ),
            (
                "distinct Unicode normalization".into(),
                json!({"é":1,"e\u{0301}":2}),
            ),
            (
                "nested scalar values".into(),
                json!({"a":[null,true,"text",0.1]}),
            ),
            (
                "signed integer zero".into(),
                Value::Number(Number::from(0_i64)),
            ),
            (
                "unsigned integer zero".into(),
                Value::Number(Number::from(0_u64)),
            ),
            ("float zero".into(), json!(0.0)),
            ("negative float zero".into(), json!(-0.0)),
            ("positive integral float".into(), json!(1.0)),
            ("negative integral float".into(), json!(-1.0)),
            (
                "safe positive integer".into(),
                json!(9_007_199_254_740_991_u64),
            ),
            (
                "safe negative integer".into(),
                json!(-9_007_199_254_740_991_i64),
            ),
            ("safe integral float".into(), json!(9_007_199_254_740_991.0)),
            (
                "unsafe positive integer".into(),
                json!(9_007_199_254_740_992_u64),
            ),
            (
                "unsafe negative integer".into(),
                json!(-9_007_199_254_740_992_i64),
            ),
            ("maximum unsigned integer".into(), json!(u64::MAX)),
            ("minimum signed integer".into(), json!(i64::MIN)),
            ("unsafe large float".into(), json!(1e100)),
            ("positive fraction".into(), json!(0.1)),
            ("negative fraction".into(), json!(-0.1)),
            ("fractional rounding".into(), json!(333_333_333.333_333_3)),
            ("small exponent".into(), json!(1e-7)),
            ("decimal exponent threshold".into(), json!(1e-6)),
            ("positive subnormal".into(), json!(f64::from_bits(1))),
            ("negative subnormal".into(), json!(-f64::from_bits(1))),
            (
                "nested unsafe integer".into(),
                json!({"a":[9_007_199_254_740_992_u64]}),
            ),
        ];
        for depth in [127, 128, 129] {
            for kind in ["array", "object", "mixed"] {
                let mut value = json!(0);
                for level in 0..depth {
                    value = if kind == "array" || (kind == "mixed" && level % 2 == 0) {
                        json!([value])
                    } else {
                        json!({"n":value})
                    };
                }
                cases.push((format!("{kind} container depth {depth}"), value));
            }
        }
        for size in [1_048_575, 1_048_576, 1_048_577] {
            cases.push((
                format!("{size} serialized bytes"),
                json!("a".repeat(size - 2)),
            ));
        }
        for (name, source) in [
            (
                "v1",
                include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../testing/fixtures/contracts/v1.json"
                )),
            ),
            (
                "review-v1",
                include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../testing/fixtures/contracts/review-v1.json"
                )),
            ),
        ] {
            let fixture: Value = serde_json::from_str(source).unwrap();
            for case in fixture["canonical"].as_array().unwrap() {
                cases.push((
                    format!(
                        "shared {name} canonical: {}",
                        case["name"].as_str().unwrap()
                    ),
                    case["value"].clone(),
                ));
            }
        }
        cases
    }

    fn check(label: &str, value: &Value) {
        let before = value.clone();
        let expected = crate::canonical_json(value).map(|bytes| bytes.len());
        let actual = canonical_json_len(value);
        assert_eq!(actual, expected, "canonical length/error: {label}");
        assert_eq!(value, &before, "borrowed value mutated: {label}");
        assert_eq!(
            serde_json::to_vec(value).unwrap(),
            serde_json::to_vec(&before).unwrap(),
            "borrowed representation changed: {label}"
        );
    }

    #[test]
    fn length_matches_allocating_jcs_for_existing_borrowed_corpus_and_boundaries() {
        let cases = values();
        assert_eq!(cases.len(), 48, "retained borrowed-value corpus changed");
        for (label, value) in cases {
            check(&label, &value);
        }
        for number in [
            -0.0,
            1.0,
            1e-6,
            1e-7,
            5e-324,
            1.7976931348623157e308,
            0.00027,
            333_333_333.333_333_3,
        ] {
            check("binary64 scalar", &json!(number));
        }
        check(
            "all JSON control escapes",
            &json!(
                (0..32)
                    .map(|n| char::from_u32(n).unwrap())
                    .collect::<String>()
            ),
        );
        check(
            "object key escapes and supplementary Unicode",
            &json!({"\"\\\n\u{0000}":[null,true,false],"😀":"é中","\u{e000}":1}),
        );
        for length in [0, 1, 1_048_575, 1_048_576, 1_048_577] {
            check(
                "exact encoded length boundaries",
                &Value::String("x".repeat(length)),
            );
        }
    }

    #[test]
    fn portability_refusal_order_matches_the_allocating_oracle() {
        let mut too_deep = Value::Null;
        for _ in 0..129 {
            too_deep = json!([too_deep]);
        }
        let unsafe_integer = json!(9_007_199_254_740_992_u64);
        check(
            "depth before number",
            &json!([too_deep.clone(), unsafe_integer.clone()]),
        );
        check("number before depth", &json!([unsafe_integer, too_deep]));
    }
    #[test]
    fn single_preflight_matches_frozen_portability_and_selects_exact_output() {
        fn numeric(value: &Value) -> bool {
            match value {
                Value::Number(number) => !number.is_i64() && !number.is_u64(),
                Value::Array(values) => values.iter().any(numeric),
                Value::Object(values) => values.values().any(numeric),
                _ => false,
            }
        }
        let mut cases = values();
        let mut deep = Value::Null;
        for _ in 0..129 {
            deep = json!([deep]);
        }
        cases.push((
            "numeric before unsafe".into(),
            json!([0.1, 9_007_199_254_740_992_u64]),
        ));
        cases.push(("numeric before depth".into(), json!([0.1, deep])));
        for (label, value) in cases {
            let expected = super::super::frozen_portable(&value, 0);
            assert_eq!(
                super::super::portable(&value, 0),
                expected,
                "ordinary validator changed: {label}"
            );
            let actual = super::decode_preflight(&value);
            match expected {
                Err(error) => match actual {
                    Err(actual) => assert_eq!(actual, error, "{label}"),
                    Ok(_) => panic!("accepted invalid {label}"),
                },
                Ok(()) => {
                    let encoded = serde_jcs::to_string(&value).unwrap();
                    match actual.unwrap() {
                        super::DecodePreflight::Bytes(bytes) => {
                            assert!(numeric(&value), "unexpected bytes: {label}");
                            assert_eq!(bytes, encoded);
                        }
                        super::DecodePreflight::Length(length) => {
                            assert!(!numeric(&value), "numeric bytes lost: {label}");
                            assert_eq!(length, encoded.len());
                        }
                    }
                }
            }
        }
    }
}
