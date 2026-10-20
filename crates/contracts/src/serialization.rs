use serde::{Serialize, Serializer, ser};
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
