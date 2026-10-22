use serde::{Serialize, Serializer, de::DeserializeOwned, ser};
use serde_json::Value;

use crate::{ContractError, Result, validate};

/// Serializes once using Serde's JSON shape, refusing non-finite numbers before JSON loses them.
///
/// Custom serializers run once and retain their ordinary human-readable representation.
/// A serializer that explicitly chooses a string or null produces that value; this operation
/// rejects non-finite values passed to Serde's floating-point serialization methods.
///
/// # Errors
///
/// Returns `serialization` for a custom serializer failure or a non-finite floating-point
/// value, including nested values and map keys. Returns a portable-profile error when the
/// resulting JSON is outside the supported value domain. The borrowed input is not mutated
/// by this operation; effects performed by its custom serializer cannot be rolled back.
///
/// ```
/// use serde_json::json;
/// use tembra_contracts::to_portable_value;
///
/// assert_eq!(to_portable_value(&vec![Some(1.5_f64), None])?, json!([1.5, null]));
/// let value = vec![Some(f64::NAN)];
/// assert_eq!(to_portable_value(&value).unwrap_err().reason, "serialization");
/// assert!(value[0].unwrap().is_nan());
/// assert_eq!(to_portable_value(&Vec::<f64>::new())?, json!([]));
/// # Ok::<(), tembra_contracts::ContractError>(())
/// ```
pub fn to_portable_value<T: Serialize + ?Sized>(value: &T) -> Result<Value> {
    let value = value
        .serialize(FiniteSerializer(serde_json::value::Serializer))
        .map_err(|error| ContractError::new("serialization", error.to_string()))?;
    validate(&Value::Bool(true), &value)?;
    Ok(value)
}

/// Decodes portable JSON after normalizing safe mathematical integers for Serde.
///
/// Integral binary64 values from JavaScript, including values beyond the i32 range, decode
/// into Rust integer fields as their mathematical value. The borrowed input is unchanged.
///
/// # Errors
/// Returns a portable-profile error for unsafe numbers or excessive nesting, or
/// `deserialization` when the normalized value does not fit `T`. Fractional values are never
/// truncated into integer fields. Effects of a custom deserializer cannot be rolled back.
///
/// ```
/// use serde_json::json;
/// use tembra_contracts::from_portable_value;
///
/// let value = json!({"wake_at_ms": 1_792_536_326_221.0});
/// let decoded: std::collections::BTreeMap<String, u64> = from_portable_value(&value)?;
/// assert_eq!(decoded["wake_at_ms"], 1_792_536_326_221);
/// assert_eq!(value["wake_at_ms"], json!(1_792_536_326_221.0));
/// assert_eq!(from_portable_value::<u64>(&json!(1.5)).unwrap_err().reason, "deserialization");
/// # Ok::<(), tembra_contracts::ContractError>(())
/// ```
pub fn from_portable_value<T: DeserializeOwned>(value: &Value) -> Result<T> {
    let encoded = crate::canonical_json(value)?;
    if encoded.len() > 1_048_576 {
        return Err(ContractError::new(
            "json_size",
            "JSON input exceeds the one MiB ingestion limit",
        ));
    }
    // Optional arbitrary_precision changes how the strict parser presents number
    // tokens. Preserve that existing boundary for floating representations rather
    // than silently accepting different values or custom-deserializer errors.
    if needs_numeric_roundtrip(value) {
        let normalized = crate::parse_json(&encoded)?;
        return serde_json::from_value(normalized)
            .map_err(|error| ContractError::new("deserialization", error.to_string()));
    }
    let mut normalized = value.clone();
    crate::json::normalize_numbers(&mut normalized);
    normalize_object_order(&mut normalized);
    serde_json::from_value(normalized)
        .map_err(|error| ContractError::new("deserialization", error.to_string()))
}

fn needs_numeric_roundtrip(value: &Value) -> bool {
    match value {
        Value::Number(number) => !number.is_i64() && !number.is_u64(),
        Value::Array(values) => values.iter().any(needs_numeric_roundtrip),
        Value::Object(values) => values.values().any(needs_numeric_roundtrip),
        _ => false,
    }
}

// Parsing canonical JSON visits object keys in UTF-16 order when preserve_order is
// unified by a consumer. Rebuild only unordered maps; default BTreeMap parsing still
// observes its normal key order. No custom deserializer executes during preparation.
fn normalize_object_order(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if !map
                .keys()
                .zip(map.keys().skip(1))
                .all(|(a, b)| a.encode_utf16().cmp(b.encode_utf16()).is_le())
            {
                let mut entries: Vec<_> = std::mem::take(map).into_iter().collect();
                entries.sort_by(|(a, _), (b, _)| a.encode_utf16().cmp(b.encode_utf16()));
                map.extend(entries);
            }
            for value in map.values_mut() {
                normalize_object_order(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                normalize_object_order(value);
            }
        }
        _ => {}
    }
}

struct Checked<'a, T: ?Sized>(&'a T);

impl<T: Serialize + ?Sized> Serialize for Checked<'_, T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        self.0.serialize(FiniteSerializer(serializer))
    }
}

struct FiniteSerializer<S>(S);
struct CheckedCompound<C>(C);

macro_rules! delegate_scalar {
    ($($method:ident($ty:ty)),* $(,)?) => {
        $(
            fn $method(self, value: $ty) -> std::result::Result<Self::Ok, Self::Error> {
                self.0.$method(value)
            }
        )*
    };
}

impl<S: Serializer> Serializer for FiniteSerializer<S> {
    type Ok = S::Ok;
    type Error = S::Error;
    type SerializeSeq = CheckedCompound<S::SerializeSeq>;
    type SerializeTuple = CheckedCompound<S::SerializeTuple>;
    type SerializeTupleStruct = CheckedCompound<S::SerializeTupleStruct>;
    type SerializeTupleVariant = CheckedCompound<S::SerializeTupleVariant>;
    type SerializeMap = CheckedCompound<S::SerializeMap>;
    type SerializeStruct = CheckedCompound<S::SerializeStruct>;
    type SerializeStructVariant = CheckedCompound<S::SerializeStructVariant>;

    delegate_scalar! {
        serialize_bool(bool),
        serialize_i8(i8),
        serialize_i16(i16),
        serialize_i32(i32),
        serialize_i64(i64),
        serialize_i128(i128),
        serialize_u8(u8),
        serialize_u16(u16),
        serialize_u32(u32),
        serialize_u64(u64),
        serialize_u128(u128),
        serialize_char(char),
        serialize_str(&str),
        serialize_bytes(&[u8]),
    }

    fn serialize_f32(self, value: f32) -> std::result::Result<Self::Ok, Self::Error> {
        if !value.is_finite() {
            return Err(ser::Error::custom(
                "non-finite number has no portable JSON encoding",
            ));
        }
        self.0.serialize_f32(value)
    }

    fn serialize_f64(self, value: f64) -> std::result::Result<Self::Ok, Self::Error> {
        if !value.is_finite() {
            return Err(ser::Error::custom(
                "non-finite number has no portable JSON encoding",
            ));
        }
        self.0.serialize_f64(value)
    }

    fn serialize_none(self) -> std::result::Result<Self::Ok, Self::Error> {
        self.0.serialize_none()
    }

    fn serialize_some<T: Serialize + ?Sized>(
        self,
        value: &T,
    ) -> std::result::Result<Self::Ok, Self::Error> {
        self.0.serialize_some(&Checked(value))
    }

    fn serialize_unit(self) -> std::result::Result<Self::Ok, Self::Error> {
        self.0.serialize_unit()
    }

    fn serialize_unit_struct(
        self,
        name: &'static str,
    ) -> std::result::Result<Self::Ok, Self::Error> {
        self.0.serialize_unit_struct(name)
    }

    fn serialize_unit_variant(
        self,
        name: &'static str,
        index: u32,
        variant: &'static str,
    ) -> std::result::Result<Self::Ok, Self::Error> {
        self.0.serialize_unit_variant(name, index, variant)
    }

    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        name: &'static str,
        value: &T,
    ) -> std::result::Result<Self::Ok, Self::Error> {
        self.0.serialize_newtype_struct(name, &Checked(value))
    }

    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        name: &'static str,
        index: u32,
        variant: &'static str,
        value: &T,
    ) -> std::result::Result<Self::Ok, Self::Error> {
        self.0
            .serialize_newtype_variant(name, index, variant, &Checked(value))
    }

    fn serialize_seq(
        self,
        len: Option<usize>,
    ) -> std::result::Result<Self::SerializeSeq, Self::Error> {
        self.0.serialize_seq(len).map(CheckedCompound)
    }

    fn serialize_tuple(self, len: usize) -> std::result::Result<Self::SerializeTuple, Self::Error> {
        self.0.serialize_tuple(len).map(CheckedCompound)
    }

    fn serialize_tuple_struct(
        self,
        name: &'static str,
        len: usize,
    ) -> std::result::Result<Self::SerializeTupleStruct, Self::Error> {
        self.0
            .serialize_tuple_struct(name, len)
            .map(CheckedCompound)
    }

    fn serialize_tuple_variant(
        self,
        name: &'static str,
        index: u32,
        variant: &'static str,
        len: usize,
    ) -> std::result::Result<Self::SerializeTupleVariant, Self::Error> {
        self.0
            .serialize_tuple_variant(name, index, variant, len)
            .map(CheckedCompound)
    }

    fn serialize_map(
        self,
        len: Option<usize>,
    ) -> std::result::Result<Self::SerializeMap, Self::Error> {
        self.0.serialize_map(len).map(CheckedCompound)
    }

    fn serialize_struct(
        self,
        name: &'static str,
        len: usize,
    ) -> std::result::Result<Self::SerializeStruct, Self::Error> {
        self.0.serialize_struct(name, len).map(CheckedCompound)
    }

    fn serialize_struct_variant(
        self,
        name: &'static str,
        index: u32,
        variant: &'static str,
        len: usize,
    ) -> std::result::Result<Self::SerializeStructVariant, Self::Error> {
        self.0
            .serialize_struct_variant(name, index, variant, len)
            .map(CheckedCompound)
    }

    fn collect_str<T: std::fmt::Display + ?Sized>(
        self,
        value: &T,
    ) -> std::result::Result<Self::Ok, Self::Error> {
        self.0.collect_str(value)
    }

    fn is_human_readable(&self) -> bool {
        self.0.is_human_readable()
    }
}

macro_rules! checked_elements {
    ($trait:ident, $method:ident) => {
        impl<C: ser::$trait> ser::$trait for CheckedCompound<C> {
            type Ok = C::Ok;
            type Error = C::Error;

            fn $method<T: Serialize + ?Sized>(
                &mut self,
                value: &T,
            ) -> std::result::Result<(), Self::Error> {
                self.0.$method(&Checked(value))
            }

            fn end(self) -> std::result::Result<Self::Ok, Self::Error> {
                self.0.end()
            }
        }
    };
}

checked_elements!(SerializeSeq, serialize_element);
checked_elements!(SerializeTuple, serialize_element);
checked_elements!(SerializeTupleStruct, serialize_field);
checked_elements!(SerializeTupleVariant, serialize_field);

impl<C: ser::SerializeMap> ser::SerializeMap for CheckedCompound<C> {
    type Ok = C::Ok;
    type Error = C::Error;

    fn serialize_key<T: Serialize + ?Sized>(
        &mut self,
        key: &T,
    ) -> std::result::Result<(), Self::Error> {
        self.0.serialize_key(&Checked(key))
    }

    fn serialize_value<T: Serialize + ?Sized>(
        &mut self,
        value: &T,
    ) -> std::result::Result<(), Self::Error> {
        self.0.serialize_value(&Checked(value))
    }

    fn serialize_entry<K: Serialize + ?Sized, V: Serialize + ?Sized>(
        &mut self,
        key: &K,
        value: &V,
    ) -> std::result::Result<(), Self::Error> {
        self.0.serialize_entry(&Checked(key), &Checked(value))
    }

    fn end(self) -> std::result::Result<Self::Ok, Self::Error> {
        self.0.end()
    }
}

macro_rules! checked_fields {
    ($trait:ident) => {
        impl<C: ser::$trait> ser::$trait for CheckedCompound<C> {
            type Ok = C::Ok;
            type Error = C::Error;

            fn serialize_field<T: Serialize + ?Sized>(
                &mut self,
                key: &'static str,
                value: &T,
            ) -> std::result::Result<(), Self::Error> {
                self.0.serialize_field(key, &Checked(value))
            }

            fn skip_field(&mut self, key: &'static str) -> std::result::Result<(), Self::Error> {
                self.0.skip_field(key)
            }

            fn end(self) -> std::result::Result<Self::Ok, Self::Error> {
                self.0.end()
            }
        }
    };
}

checked_fields!(SerializeStruct);
checked_fields!(SerializeStructVariant);

#[cfg(test)]
mod tests {
    use std::{cell::Cell, collections::BTreeMap};

    use serde::ser::{SerializeMap, SerializeSeq, SerializeStruct, SerializeTupleVariant};
    use serde_json::json;

    use super::*;

    fn legacy_decode<T: DeserializeOwned>(value: &Value) -> Result<T> {
        let encoded = crate::canonical_json(value)?;
        let normalized = crate::parse_json(&encoded)?;
        serde_json::from_value(normalized)
            .map_err(|e| ContractError::new("deserialization", e.to_string()))
    }

    fn compare_decode<T: DeserializeOwned + std::fmt::Debug + PartialEq>(value: &Value) {
        let fingerprint = |error: ContractError| (error.reason, error.path, error.message);
        assert_eq!(
            from_portable_value::<T>(value).map_err(fingerprint),
            legacy_decode::<T>(value).map_err(fingerprint),
            "input: {value:?}"
        );
    }

    #[test]
    fn structured_decode_matches_legacy_for_precision_sensitive_number_tokens() {
        for text in [
            "1.00000000000000001",
            "9007199254740991.01",
            "0.100000000000000000000001",
            "1e-400",
            "-1e-400",
            "4.9406564584124654e-324",
            "1.0",
            "-0.0",
        ] {
            let number: serde_json::Number = text.parse().unwrap();
            let value = Value::Number(number);
            compare_decode::<Value>(&value);
            compare_decode::<u64>(&value);
            compare_decode::<f64>(&value);
        }
    }

    #[test]
    fn structured_decode_preserves_canonical_map_visitation_and_first_error() {
        #[derive(Debug, PartialEq)]
        struct Order(Vec<(String, Vec<String>)>);
        impl<'de> serde::Deserialize<'de> for Order {
            fn deserialize<D: serde::Deserializer<'de>>(
                deserializer: D,
            ) -> std::result::Result<Self, D::Error> {
                struct Visitor;
                impl<'de> serde::de::Visitor<'de> for Visitor {
                    type Value = Order;
                    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                        f.write_str("object")
                    }
                    fn visit_map<M: serde::de::MapAccess<'de>>(
                        self,
                        mut map: M,
                    ) -> std::result::Result<Order, M::Error> {
                        let mut keys = Vec::new();
                        while let Some((key, value)) = map.next_entry::<String, Value>()? {
                            let nested = value
                                .as_object()
                                .map(|map| map.keys().cloned().collect())
                                .unwrap_or_default();
                            keys.push((key, nested));
                        }
                        Ok(Order(keys))
                    }
                }
                deserializer.deserialize_map(Visitor)
            }
        }
        #[derive(Debug, PartialEq)]
        struct Reject;
        impl<'de> serde::Deserialize<'de> for Reject {
            fn deserialize<D: serde::Deserializer<'de>>(
                deserializer: D,
            ) -> std::result::Result<Self, D::Error> {
                struct Visitor;
                impl<'de> serde::de::Visitor<'de> for Visitor {
                    type Value = Reject;
                    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                        f.write_str("object")
                    }
                    fn visit_map<M: serde::de::MapAccess<'de>>(
                        self,
                        mut map: M,
                    ) -> std::result::Result<Reject, M::Error> {
                        let first = map.next_key::<String>()?.unwrap();
                        Err(serde::de::Error::custom(format!("first field: {first}")))
                    }
                }
                deserializer.deserialize_map(Visitor)
            }
        }
        let value: Value = serde_json::from_str(
            r#"{"z":{},"\ue000":{"z":0,"a":1},"😀":{"\ue000":0,"😀":1,"a":2},"a":{}}"#,
        )
        .unwrap();
        let untouched = value.clone();
        compare_decode::<Order>(&value);
        compare_decode::<Reject>(&value);
        let expected = crate::parse_json(&crate::canonical_json(&value).unwrap()).unwrap();
        assert_eq!(
            from_portable_value::<Order>(&value)
                .unwrap()
                .0
                .iter()
                .map(|(key, _)| key)
                .collect::<Vec<_>>(),
            expected.as_object().unwrap().keys().collect::<Vec<_>>()
        );
        assert_eq!(value, untouched);
    }

    #[test]
    fn structured_decode_matches_legacy_numeric_and_structured_values() {
        let fixtures = [
            json!(null),
            json!(true),
            json!(false),
            json!(0),
            json!(-0.0),
            json!(1.0),
            json!(-17.0),
            json!(1.25),
            json!(0.1),
            json!(1.2345678901234567),
            json!(1.0e-308),
            json!(f64::from_bits(1)),
            json!(9_007_199_254_740_991_u64),
            json!(-9_007_199_254_740_991_i64),
            json!(9_007_199_254_740_992_u64),
            json!(u64::MAX),
            json!(1.0e100),
            json!("quote\" slash\\ newline\n \u{0} é 🦀"),
            json!({"nested":[1.0,-0.0,1.25,null,{"x":"\\uD800","é":true}]}),
        ];
        for value in &fixtures {
            let before = value.clone();
            compare_decode::<Value>(value);
            compare_decode::<u64>(value);
            compare_decode::<i64>(value);
            compare_decode::<f64>(value);
            assert_eq!(*value, before);
        }
        let mut bits = 0x9e3779b97f4a7c15_u64;
        for _ in 0..4096 {
            bits ^= bits << 13;
            bits ^= bits >> 7;
            bits ^= bits << 17;
            if let Some(number) = serde_json::Number::from_f64(f64::from_bits(bits)) {
                compare_decode::<Value>(&Value::Number(number));
            }
        }
    }

    #[test]
    fn structured_decode_preserves_encoded_size_and_depth_refusals() {
        for length in [1_048_573, 1_048_574, 1_048_575] {
            compare_decode::<Value>(&Value::String("a".repeat(length)));
        }
        compare_decode::<Value>(&Value::String("\u{0}".repeat(174_763)));
        for depth in [127, 128, 129] {
            let mut value = json!(null);
            for _ in 0..depth {
                value = json!([value]);
            }
            compare_decode::<Value>(&value);
        }
    }

    #[test]
    fn structured_decode_calls_the_current_custom_deserializer_exactly_once() {
        thread_local! { static CALLS: Cell<usize> = const { Cell::new(0) }; }
        #[derive(Debug, PartialEq)]
        struct Current(Value);
        impl<'de> serde::Deserialize<'de> for Current {
            fn deserialize<D: serde::Deserializer<'de>>(
                decoder: D,
            ) -> std::result::Result<Self, D::Error> {
                CALLS.with(|calls| calls.set(calls.get() + 1));
                assert!(decoder.is_human_readable());
                let value = Value::deserialize(decoder)?;
                if value == json!("reject") {
                    return Err(serde::de::Error::custom("custom refusal"));
                }
                Ok(Self(value))
            }
        }
        for value in [
            json!(1.0),
            json!(2.5),
            json!("reject"),
            json!(9_007_199_254_740_992_u64),
        ] {
            CALLS.with(|calls| calls.set(0));
            let old = legacy_decode::<Current>(&value).map_err(|e| (e.reason, e.path, e.message));
            let previous = CALLS.with(Cell::get);
            CALLS.with(|calls| calls.set(0));
            let current =
                from_portable_value::<Current>(&value).map_err(|e| (e.reason, e.path, e.message));
            assert_eq!(current, old);
            assert_eq!(CALLS.with(Cell::get), previous);
            assert_eq!(
                previous,
                usize::from(value != json!(9_007_199_254_740_992_u64))
            );
        }
    }

    #[test]
    fn non_finite_values_are_refused_at_nested_serde_boundaries() {
        #[derive(Serialize)]
        enum Nested {
            Newtype(f64),
            Tuple(u8, f64),
            Struct { value: f64 },
        }
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            for nested in [
                Nested::Newtype(value),
                Nested::Tuple(0, value),
                Nested::Struct { value },
            ] {
                assert_eq!(
                    to_portable_value(&nested).unwrap_err().reason,
                    "serialization"
                );
            }
            assert!(to_portable_value(&Some(vec![BTreeMap::from([("value", value)])])).is_err());
            assert!(to_portable_value(&(0, Some(value))).is_err());
            assert!(to_portable_value(&(value as f32)).is_err());
        }
    }

    #[test]
    fn custom_serialization_shape_and_single_execution_are_preserved() {
        struct Once<'a>(&'a Cell<u32>);
        impl Serialize for Once<'_> {
            fn serialize<S: Serializer>(
                &self,
                serializer: S,
            ) -> std::result::Result<S::Ok, S::Error> {
                self.0.set(self.0.get() + 1);
                assert!(serializer.is_human_readable());
                let mut fields = serializer.serialize_struct("Chosen", 1)?;
                fields.serialize_field("renamed", &Some(1.5_f64))?;
                fields.end()
            }
        }
        let count = Cell::new(0);
        assert_eq!(
            to_portable_value(&Once(&count)).unwrap(),
            json!({"renamed": 1.5})
        );
        assert_eq!(count.get(), 1);
    }

    #[test]
    fn custom_compound_serializers_cannot_bypass_float_checks() {
        struct Sequence;
        impl Serialize for Sequence {
            fn serialize<S: Serializer>(
                &self,
                serializer: S,
            ) -> std::result::Result<S::Ok, S::Error> {
                let mut seq = serializer.serialize_seq(Some(1))?;
                seq.serialize_element(&f64::INFINITY)?;
                seq.end()
            }
        }
        struct Map;
        impl Serialize for Map {
            fn serialize<S: Serializer>(
                &self,
                serializer: S,
            ) -> std::result::Result<S::Ok, S::Error> {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_key("value")?;
                map.serialize_value(&f64::NAN)?;
                map.end()
            }
        }
        struct Variant;
        impl Serialize for Variant {
            fn serialize<S: Serializer>(
                &self,
                serializer: S,
            ) -> std::result::Result<S::Ok, S::Error> {
                let mut fields = serializer.serialize_tuple_variant("Enum", 0, "Variant", 1)?;
                fields.serialize_field(&f32::NEG_INFINITY)?;
                fields.end()
            }
        }
        assert!(to_portable_value(&Sequence).is_err());
        assert!(to_portable_value(&Map).is_err());
        assert!(to_portable_value(&Variant).is_err());
    }
}
