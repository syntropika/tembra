//! Portable values use the same tagged JSON convention as the JavaScript SDK.
//! Custom infrastructure codecs may replace this wire policy without changing the kernel.
use crate::core::Reason;
use crate::{Error, Result};
use serde_json::{Value, json};

pub trait Codec {
    fn id(&self) -> &str;
    fn serialize(&self, value: &Value) -> Result<Vec<u8>>;
    fn deserialize(&self, bytes: &[u8]) -> Result<Value>;
}
pub struct Json;
impl Codec for Json {
    fn id(&self) -> &str {
        "tembra-json/1"
    }
    fn serialize(&self, value: &Value) -> Result<Vec<u8>> {
        encode(value)
    }
    fn deserialize(&self, bytes: &[u8]) -> Result<Value> {
        decode(bytes)
    }
}

/// Portable decimal integer. Rust callers can apply checked conversion to their chosen width.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Integer(pub String);
impl serde::Serialize for Integer {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        bigint(self.0.clone())
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
}
impl<'de> serde::Deserialize<'de> for Integer {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let tagged = &value["$tembra"];
        if tagged[0] != "bigint" {
            return Err(serde::de::Error::custom("expected a portable BigInt"));
        }
        let decimal = tagged[1]
            .as_str()
            .ok_or_else(|| serde::de::Error::custom("expected decimal text"))?;
        bigint(decimal).map_err(serde::de::Error::custom)?;
        Ok(Self(decimal.into()))
    }
}

pub fn bigint(decimal: impl Into<String>) -> Result<Value> {
    let decimal = decimal.into();
    let digits = decimal.strip_prefix('-').unwrap_or(&decimal);
    if digits.is_empty()
        || (digits.len() > 1 && digits.starts_with('0'))
        || !digits.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(Error::invalid(Reason::Bigint, "expected a decimal integer"));
    }
    Ok(json!({"$tembra": ["bigint", decimal]}))
}
pub fn bytes(value: &[u8]) -> Value {
    json!({"$tembra": ["bytes", value]})
}
pub fn encode(value: &Value) -> Result<Vec<u8>> {
    serde_json::to_vec(value)
        .map_err(|error| Error::invalid(Reason::Serialization, error.to_string()))
}
pub fn decode(bytes: &[u8]) -> Result<Value> {
    serde_json::from_slice(bytes)
        .map_err(|error| Error::invalid(Reason::Serialization, error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portable_integer_text_matches_javascript() {
        for decimal in [
            "0",
            "-0",
            "7",
            "-7",
            "9007199254740993",
            "-18446744073709551616",
        ] {
            let integer = Integer(decimal.into());
            let wire = serde_json::to_value(&integer).unwrap();
            assert_eq!(serde_json::from_value::<Integer>(wire).unwrap(), integer);
        }
        for decimal in ["", "-", "+7", "007", "-007", "00", "-00", "1.0", " 7", "７"] {
            assert!(bigint(decimal).is_err(), "accepted {decimal:?}");
            assert!(serde_json::to_value(Integer(decimal.into())).is_err());
            assert!(
                serde_json::from_value::<Integer>(json!({"$tembra": ["bigint", decimal]})).is_err()
            );
        }
    }
}
