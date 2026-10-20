use std::fmt;

use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};

use crate::{ContractError, Result};

/// Parse a portable JSON value without losing duplicate object keys or unsafe integers.
///
/// # Errors
/// Rejects inputs over one MiB, invalid JSON/Unicode, duplicate keys at any depth,
/// unsafe integral values, nonfinite values, fractional tokens that round to an
/// integer/zero, and excessive nesting. Ordinary fractional rounding is permitted. No partial
/// value is returned; this function performs no external effects.
pub fn parse_json(input: &str) -> Result<Value> {
    if input.len() > 1_048_576 {
        return Err(ContractError::new(
            "json_size",
            "JSON input exceeds the one MiB ingestion limit",
        ));
    }
    check_number_tokens(input)?;
    let mut decoder = serde_json::Deserializer::from_str(input);
    // The lexical pass bounds containers before recursion. Serde's default limit
    // otherwise rejects the 128th container, one earlier than this profile.
    decoder.disable_recursion_limit();
    let value = StrictValue::deserialize(&mut decoder)
        .map_err(|error| ContractError::new("json", error.to_string()))?
        .0;
    decoder
        .end()
        .map_err(|error| ContractError::new("json", error.to_string()))?;
    crate::canonical_json(&value)?;
    Ok(value)
}

// JSON deserializers normally lose decimal spelling before visiting f64. Inspect
// tokens first so unsafe integers and fractional-to-integral collapse cannot hide
// behind that rounding. This is not a second JSON parser: serde still validates
// grammar, strings, trailing input, and nesting after this bounded lexical pass.
fn check_number_tokens(input: &str) -> Result<()> {
    let bytes = input.as_bytes();
    let mut index = 0;
    let mut quoted = false;
    let mut containers = 0_usize;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' if quoted => {
                index += 2;
                continue;
            }
            b'"' => quoted = !quoted,
            b'[' | b'{' if !quoted => {
                containers += 1;
                if containers > 128 {
                    return Err(ContractError::new("depth", "JSON nesting limit exceeded"));
                }
            }
            b']' | b'}' if !quoted => containers = containers.saturating_sub(1),
            b'-' | b'0'..=b'9' if !quoted => {
                let start = index;
                index += 1;
                while index < bytes.len()
                    && matches!(bytes[index], b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-')
                {
                    index += 1;
                }
                check_decimal(&input[start..index])?;
                continue;
            }
            _ => {}
        }
        index += 1;
    }
    Ok(())
}

fn check_decimal(token: &str) -> Result<()> {
    let parsed = token
        .parse::<f64>()
        .map_err(|_| ContractError::new("json", "invalid numeric token"))?;
    if !parsed.is_finite() {
        return Err(ContractError::new("number", "non-finite numeric token"));
    }
    let unsigned = token.strip_prefix('-').unwrap_or(token);
    let (mantissa, exponent) = unsigned.split_once(['e', 'E']).unwrap_or((unsigned, "0"));
    let exponent = exponent
        .parse::<i64>()
        .unwrap_or(if exponent.starts_with('-') {
            i64::MIN
        } else {
            i64::MAX
        });
    let fractional_digits = mantissa
        .split_once('.')
        .map_or(0, |(_, fraction)| fraction.len());
    let digits: String = mantissa
        .chars()
        .filter(|character| *character != '.')
        .collect();
    let digits = digits.trim_start_matches('0');
    if digits.is_empty() {
        return Ok(());
    }
    let scale = exponent.saturating_sub(fractional_digits as i64);
    let suffix = scale.unsigned_abs();
    let integral = scale >= 0
        || (suffix <= digits.len() as u64
            && digits.as_bytes()[digits.len() - suffix as usize..]
                .iter()
                .all(|digit| *digit == b'0'));
    if integral {
        let significant = if scale < 0 {
            &digits[..digits.len() - suffix as usize]
        } else {
            digits
        };
        let total_digits =
            (significant.len() as u64).saturating_add(if scale >= 0 { scale as u64 } else { 0 });
        if total_digits > 16 {
            return Err(ContractError::new(
                "number",
                "integer token is outside the safe portable range",
            ));
        }
        let mut expanded = significant.to_owned();
        if scale > 0 {
            expanded.extend(std::iter::repeat_n('0', scale as usize));
        }
        if expanded.len() == 16 && expanded.as_str() > "9007199254740991" {
            return Err(ContractError::new(
                "number",
                "integer token is outside the safe portable range",
            ));
        }
    } else if parsed.fract() == 0.0 {
        return Err(ContractError::new(
            "number",
            "fractional token collapses to an integer or zero in binary64",
        ));
    }
    Ok(())
}

struct StrictValue(Value);

impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D: Deserializer<'de>>(decoder: D) -> std::result::Result<Self, D::Error> {
        decoder.deserialize_any(StrictVisitor)
    }
}

struct StrictVisitor;

impl<'de> Visitor<'de> for StrictVisitor {
    type Value = StrictValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("strict portable JSON")
    }
    fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Self::Value, E> {
        Ok(StrictValue(Value::Null))
    }
    fn visit_bool<E: serde::de::Error>(self, value: bool) -> std::result::Result<Self::Value, E> {
        Ok(StrictValue(Value::Bool(value)))
    }
    fn visit_str<E: serde::de::Error>(self, value: &str) -> std::result::Result<Self::Value, E> {
        Ok(StrictValue(Value::String(value.into())))
    }
    fn visit_string<E: serde::de::Error>(
        self,
        value: String,
    ) -> std::result::Result<Self::Value, E> {
        Ok(StrictValue(Value::String(value)))
    }
    fn visit_i64<E: serde::de::Error>(self, value: i64) -> std::result::Result<Self::Value, E> {
        if value.unsigned_abs() > 9_007_199_254_740_991 {
            return Err(E::custom("integer outside the safe portable range"));
        }
        Ok(StrictValue(Value::Number(value.into())))
    }
    fn visit_u64<E: serde::de::Error>(self, value: u64) -> std::result::Result<Self::Value, E> {
        if value > 9_007_199_254_740_991 {
            return Err(E::custom("integer outside the safe portable range"));
        }
        Ok(StrictValue(Value::Number(value.into())))
    }
    fn visit_f64<E: serde::de::Error>(self, value: f64) -> std::result::Result<Self::Value, E> {
        if !value.is_finite() || (value.fract() == 0.0 && value.abs() > 9_007_199_254_740_991.0) {
            return Err(E::custom("number outside the finite safe portable domain"));
        }
        let number = if value.fract() == 0.0 {
            Number::from(value as i64)
        } else {
            Number::from_f64(value).ok_or_else(|| E::custom("non-finite number"))?
        };
        Ok(StrictValue(Value::Number(number)))
    }
    fn visit_seq<A: SeqAccess<'de>>(
        self,
        mut sequence: A,
    ) -> std::result::Result<Self::Value, A::Error> {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element::<StrictValue>()? {
            values.push(value.0);
        }
        Ok(StrictValue(Value::Array(values)))
    }
    fn visit_map<A: MapAccess<'de>>(
        self,
        mut object: A,
    ) -> std::result::Result<Self::Value, A::Error> {
        let mut values = Map::new();
        while let Some(name) = object.next_key::<String>()? {
            if values.contains_key(&name) {
                return Err(serde::de::Error::custom(format!(
                    "duplicate object key {name:?}"
                )));
            }
            values.insert(name, object.next_value::<StrictValue>()?.0);
        }
        Ok(StrictValue(Value::Object(values)))
    }
}

// JCS-equivalent integral floats must not remain distinguishable through Value's
// storage representation at typed descriptor and schema comparison boundaries.
// Callers establish the finite safe-number/depth profile before invoking this.
pub(crate) fn normalize_numbers(value: &mut Value) {
    match value {
        Value::Number(number) => {
            if let Some(value) = number.as_f64()
                && value.fract() == 0.0
            {
                *number = Number::from(value as i64);
            }
        }
        Value::Array(values) => {
            for value in values {
                normalize_numbers(value);
            }
        }
        Value::Object(values) => {
            for value in values.values_mut() {
                normalize_numbers(value);
            }
        }
        _ => {}
    }
}
