//! Strict, bounded decoding of command inputs.
//!
//! A platform value (Node-API object, browser object or JSON text) is read once, through a single
//! generic serde visitor, into an [`Input`] tree. Reading charges one budget for every scalar,
//! text byte and container element and caps nesting, so structured inputs without a byte length
//! are bounded like the one-mebibyte JSON request. Generated [`Decode`] implementations then
//! extract typed command fields, refusing unknown, missing and mistyped fields.

use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::fmt;
use tembra_core::{Error, Reason, Result};

/// The same metadata allowance as the JSON adapter's one-mebibyte request limit.
pub(crate) const MAX_UNITS: usize = 1024 * 1024;
/// Current commands fit within eight containers, including nested Orb requirements.
pub(crate) const MAX_DEPTH: usize = 8;

/// A bounded, self-describing input value.
#[derive(Clone, Debug, PartialEq)]
pub enum Input {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    Text(String),
    List(Vec<Input>),
    Record(Vec<(String, Input)>),
}

impl Input {
    /// Name the shape of a value for type errors.
    fn kind(&self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Bool(_) => "a boolean",
            Self::Int(_) | Self::UInt(_) | Self::Float(_) => "a number",
            Self::Text(_) => "text",
            Self::List(_) => "an array",
            Self::Record(_) => "an object",
        }
    }
}

/// Create the structured metadata error used for every refused input.
pub(crate) fn invalid(message: impl Into<String>) -> Error {
    Error::invalid(Reason::Metadata, message)
}

/// Refuse a value whose shape does not match the expected wire type.
pub(crate) fn expected(what: &str, found: &Input) -> Error {
    invalid(format!("expected {what}, found {}", found.kind()))
}

/// Read one platform value into a bounded input tree.
pub(crate) fn read<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Input> {
    let budget = Budget {
        units: Cell::new(MAX_UNITS),
        exceeded: Cell::new(false),
    };
    Reader {
        budget: &budget,
        depth: 0,
    }
    .deserialize(deserializer)
    .map_err(|error| {
        if budget.exceeded.get() {
            Error::invalid(Reason::MetadataSize, "metadata exceeds one mebibyte")
        } else {
            invalid(error.to_string())
        }
    })
}

/// Decode string-only platforms without depending on serde_json's private numeric map format.
/// JSON is byte-bounded before parsing; numbers are read through Number's public accessors.
pub(crate) fn read_json(text: &str) -> Result<Input> {
    let value: &serde_json::value::RawValue =
        serde_json::from_str(text).map_err(|error| invalid(error.to_string()))?;
    let budget = Budget {
        units: Cell::new(MAX_UNITS),
        exceeded: Cell::new(false),
    };
    Reader {
        budget: &budget,
        depth: 0,
    }
    .json(value.get())
    .map_err(|error| {
        if budget.exceeded.get() {
            Error::invalid(Reason::MetadataSize, "metadata exceeds one mebibyte")
        } else {
            invalid(error.to_string())
        }
    })
}

struct Budget {
    units: Cell<usize>,
    exceeded: Cell<bool>,
}

impl Budget {
    /// Charge one value plus its payload bytes.
    fn spend<E: de::Error>(&self, bytes: usize) -> std::result::Result<(), E> {
        match self.units.get().checked_sub(bytes.saturating_add(1)) {
            Some(left) => {
                self.units.set(left);
                Ok(())
            }
            None => {
                self.exceeded.set(true);
                Err(E::custom("metadata exceeds one mebibyte"))
            }
        }
    }
}

#[derive(Clone, Copy)]
struct Reader<'b> {
    budget: &'b Budget,
    depth: usize,
}

impl Reader<'_> {
    /// Parse numbers explicitly and preserve object keys even with arbitrary_precision enabled.
    fn json(self, text: &str) -> std::result::Result<Input, serde_json::Error> {
        let mut deserializer = serde_json::Deserializer::from_str(text);
        match text.trim_start().as_bytes().first() {
            Some(b'{') => deserializer.deserialize_map(Json(self)),
            Some(b'[') => deserializer.deserialize_seq(Json(self)),
            Some(b'-' | b'0'..=b'9') => {
                let value: serde_json::Number = serde_json::from_str(text)?;
                if let Some(value) = value.as_u64() {
                    self.visit_u64(value)
                } else if let Some(value) = value.as_i64() {
                    self.visit_i64(value)
                } else {
                    self.visit_f64(
                        value
                            .as_f64()
                            .ok_or_else(|| de::Error::custom("number is out of range"))?,
                    )
                }
            }
            _ => self.deserialize(&mut deserializer),
        }
    }

    /// Enter one container level.
    fn nested<E: de::Error>(self) -> std::result::Result<Self, E> {
        if self.depth >= MAX_DEPTH {
            self.budget.exceeded.set(true);
            return Err(E::custom("metadata nesting is too deep"));
        }
        Ok(Self {
            depth: self.depth + 1,
            ..self
        })
    }
}

impl<'de> DeserializeSeed<'de> for Reader<'_> {
    type Value = Input;

    fn deserialize<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> std::result::Result<Input, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Reader<'_> {
    type Value = Input;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("metadata: null, a boolean, a number, text, an array or an object")
    }

    fn visit_unit<E: de::Error>(self) -> std::result::Result<Input, E> {
        self.budget.spend(0)?;
        Ok(Input::Null)
    }

    fn visit_none<E: de::Error>(self) -> std::result::Result<Input, E> {
        self.visit_unit()
    }

    fn visit_some<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> std::result::Result<Input, D::Error> {
        self.deserialize(deserializer)
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> std::result::Result<Input, E> {
        self.budget.spend(0)?;
        Ok(Input::Bool(value))
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> std::result::Result<Input, E> {
        self.budget.spend(0)?;
        Ok(Input::Int(value))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> std::result::Result<Input, E> {
        self.budget.spend(0)?;
        Ok(Input::UInt(value))
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> std::result::Result<Input, E> {
        self.budget.spend(0)?;
        Ok(Input::Float(value))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> std::result::Result<Input, E> {
        self.budget.spend(value.len())?;
        Ok(Input::Text(value.into()))
    }

    fn visit_string<E: de::Error>(self, value: String) -> std::result::Result<Input, E> {
        self.budget.spend(value.len())?;
        Ok(Input::Text(value))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Input, A::Error> {
        self.budget.spend(0)?;
        let reader = self.nested()?;
        let mut items = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(64));
        while let Some(item) = seq.next_element_seed(reader)? {
            items.push(item);
        }
        Ok(Input::List(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Input, A::Error> {
        self.budget.spend(0)?;
        let reader = self.nested()?;
        let mut fields = Vec::with_capacity(map.size_hint().unwrap_or(0).min(16));
        while let Some(key) = map.next_key::<String>()? {
            self.budget.spend(key.len())?;
            fields.push((key, map.next_value_seed(reader)?));
        }
        Ok(Input::Record(fields))
    }
}

/// JSON containers use raw values so numeric implementation markers cannot impersonate objects.
struct Json<'b>(Reader<'b>);
impl<'de> Visitor<'de> for Json<'_> {
    type Value = Input;
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON object or array")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Input, A::Error> {
        self.0.budget.spend(0)?;
        let nested = self.0.nested()?;
        let mut values = Vec::new();
        while let Some(value) = seq.next_element::<&serde_json::value::RawValue>()? {
            values.push(nested.json(value.get()).map_err(de::Error::custom)?);
        }
        Ok(Input::List(values))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Input, A::Error> {
        self.0.budget.spend(0)?;
        let nested = self.0.nested()?;
        let mut fields = Vec::new();
        while let Some(key) = map.next_key::<String>()? {
            self.0.budget.spend(key.len())?;
            let value: &serde_json::value::RawValue = map.next_value()?;
            fields.push((key, nested.json(value.get()).map_err(de::Error::custom)?));
        }
        Ok(Input::Record(fields))
    }
}

/// Typed extraction from an input tree.
pub trait Decode: Sized {
    /// Decode a present value.
    fn decode(input: Input) -> Result<Self>;

    /// The value of an absent record field, when absence is allowed.
    fn absent() -> Option<Self> {
        None
    }
}

/// The fields of one record, consumed by name and checked for leftovers.
pub struct Fields(Vec<(String, Input)>);

impl Fields {
    /// Require an object.
    pub fn new(input: Input) -> Result<Self> {
        match input {
            Input::Record(fields) => Ok(Self(fields)),
            other => Err(expected("an object", &other)),
        }
    }

    /// Take and decode one field by its Rust name, spelled in camelCase on the wire.
    pub fn take<T: Decode>(&mut self, name: &str) -> Result<T> {
        match self.0.iter().position(|(key, _)| camel_eq(name, key)) {
            Some(index) => {
                let (key, value) = self.0.swap_remove(index);
                T::decode(value).map_err(|error| within(&key, error))
            }
            None => T::absent()
                .ok_or_else(|| invalid(format!("missing field `{}`", crate::wire::camel(name)))),
        }
    }

    /// Refuse fields that the record does not declare, including duplicates.
    pub fn finish(self) -> Result<()> {
        match self.0.first() {
            None => Ok(()),
            Some((key, _)) => Err(invalid(format!("unknown field `{key}`"))),
        }
    }

    /// Take the operation name of a JSON request.
    pub(crate) fn take_op(&mut self) -> Result<String> {
        match self.0.iter().position(|(key, _)| key == "op") {
            Some(index) => match self.0.swap_remove(index).1 {
                Input::Text(op) => Ok(op),
                _ => Err(invalid("op must be text")),
            },
            None => Err(invalid("op must be text")),
        }
    }

    /// Return the remaining fields as a record.
    pub(crate) fn into_input(self) -> Input {
        Input::Record(self.0)
    }
}

/// Prefix an error with the field or element that produced it.
fn within(path: &str, error: Error) -> Error {
    Error::new(
        error.code,
        error.reason,
        format!("{path}: {}", error.message),
    )
}

/// Compare a snake_case Rust name with a camelCase wire key without allocating.
fn camel_eq(snake: &str, key: &str) -> bool {
    let mut key = key.chars();
    let mut upper = false;
    for character in snake.chars() {
        if character == '_' {
            upper = true;
            continue;
        }
        let expected = if upper {
            character.to_ascii_uppercase()
        } else {
            character
        };
        upper = false;
        if key.next() != Some(expected) {
            return false;
        }
    }
    key.next().is_none()
}

/// Generate a strict record decoder for a struct with named fields.
macro_rules! decode_record {
    ($name:ty { $($field:ident),* }) => {
        impl $crate::decode::Decode for $name {
            fn decode(input: $crate::decode::Input) -> tembra_core::Result<Self> {
                #[allow(unused_mut)]
                let mut fields = $crate::decode::Fields::new(input)?;
                let value = Self { $($field: fields.take(stringify!($field))?,)* };
                fields.finish()?;
                Ok(value)
            }
        }
    };
}

impl Decode for bool {
    fn decode(input: Input) -> Result<Self> {
        match input {
            Input::Bool(value) => Ok(value),
            other => Err(expected("a boolean", &other)),
        }
    }
}

impl Decode for String {
    fn decode(input: Input) -> Result<Self> {
        match input {
            Input::Text(value) => Ok(value),
            other => Err(expected("text", &other)),
        }
    }
}

/// Read an exact integer; fractions and floats are refused.
pub(crate) fn integer(input: &Input) -> Option<i128> {
    match *input {
        Input::Int(value) => Some(value.into()),
        Input::UInt(value) => Some(value.into()),
        _ => None,
    }
}

impl Decode for u32 {
    fn decode(input: Input) -> Result<Self> {
        integer(&input)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(|| expected("an unsigned 32-bit integer", &input))
    }
}

impl Decode for i32 {
    fn decode(input: Input) -> Result<Self> {
        integer(&input)
            .and_then(|value| i32::try_from(value).ok())
            .ok_or_else(|| expected("a signed 32-bit integer", &input))
    }
}

impl<T: Decode> Decode for Option<T> {
    fn decode(input: Input) -> Result<Self> {
        match input {
            Input::Null => Ok(None),
            other => T::decode(other).map(Some),
        }
    }

    fn absent() -> Option<Self> {
        Some(None)
    }
}

impl<T: Decode> Decode for Vec<T> {
    fn decode(input: Input) -> Result<Self> {
        match input {
            Input::List(items) => items
                .into_iter()
                .enumerate()
                .map(|(index, item)| {
                    T::decode(item).map_err(|error| within(&format!("[{index}]"), error))
                })
                .collect(),
            other => Err(expected("an array", &other)),
        }
    }
}

impl<T: Decode> Decode for BTreeMap<String, T> {
    fn decode(input: Input) -> Result<Self> {
        match input {
            Input::Record(fields) => {
                let mut map = BTreeMap::new();
                for (key, value) in fields {
                    let value = T::decode(value).map_err(|error| within(&key, error))?;
                    if map.insert(key.clone(), value).is_some() {
                        return Err(invalid(format!("duplicate key `{key}`")));
                    }
                }
                Ok(map)
            }
            other => Err(expected("an object", &other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read a JSON value through the bounded reader.
    fn decode(value: serde_json::Value) -> Result<Input> {
        read(value)
    }

    #[test]
    fn nesting_and_volume_are_bounded_for_self_describing_input() {
        let mut nested = serde_json::json!(1);
        for _ in 0..MAX_DEPTH {
            nested = serde_json::json!([nested]);
        }
        assert!(decode(nested.clone()).is_ok());
        let error = decode(serde_json::json!([nested])).unwrap_err();
        assert_eq!(error.reason, Reason::MetadataSize.as_str());
        let error = decode(serde_json::json!(vec![0; MAX_UNITS])).unwrap_err();
        assert_eq!(error.reason, Reason::MetadataSize.as_str());
        assert!(decode(serde_json::json!(vec![0; MAX_UNITS / 2 - 1])).is_ok());
    }

    #[test]
    fn json_preserves_fractional_numbers_duplicates_and_numeric_looking_objects() {
        let Input::Record(values) = read_json(r#"{"value":0.5}"#).unwrap() else {
            panic!("expected a record");
        };
        assert_eq!(values[0].1, Input::Float(0.5));
        struct Number {
            value: u32,
        }
        decode_record!(Number { value });
        let decoded = Number::decode(read_json(r#"{"value":1}"#).unwrap()).unwrap();
        assert_eq!(decoded.value, 1);
        for input in [
            r#"{"value":0.5}"#,
            r#"{"value":1,"value":2}"#,
            r#"{"value":{"$serde_json::private::Number":"1"}}"#,
            r#"{"value":{"$serde_json::private::RawValue":"1"}}"#,
        ] {
            assert!(
                Number::decode(read_json(input).unwrap()).is_err(),
                "{input}"
            );
        }
    }

    #[test]
    fn records_refuse_positional_unknown_duplicate_and_missing_fields() {
        struct Pair {
            first_value: u32,
            second: Option<u32>,
        }
        decode_record!(Pair {
            first_value,
            second
        });
        let pair = |value| Pair::decode(read(value).unwrap());
        let ok = pair(serde_json::json!({"firstValue": 1})).unwrap();
        assert_eq!((ok.first_value, ok.second), (1, None));
        for value in [
            serde_json::json!([1, 2]),
            serde_json::json!({"first_value": 1}),
            serde_json::json!({"firstValue": 1, "extra": 2}),
            serde_json::json!({"second": 2}),
            serde_json::json!({"firstValue": 1.0}),
            serde_json::json!({"firstValue": "1"}),
            serde_json::json!({"firstValue": 4_294_967_296_u64}),
        ] {
            assert!(pair(value.clone()).is_err(), "{value}");
        }
        let duplicate = Input::Record(vec![
            ("firstValue".into(), Input::UInt(1)),
            ("firstValue".into(), Input::UInt(2)),
        ]);
        assert_eq!(
            Pair::decode(duplicate).err().unwrap().message,
            "unknown field `firstValue`"
        );
    }
}
