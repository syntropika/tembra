//! Convert the bounded portable profile to actual Component Model ABI values.
use crate::failure;
use serde_json::{Value, json};
use tembra::Result;
use tembra_contracts::javascript_name;
use wasmtime::component::{ResourceAny, ResourceType, Type, Val};

/// Lower already WIT-validated data using the executable component's own parameter types.
pub(super) fn lower(ty: Type, value: &Value) -> Result<Val> {
    lower_with(ty, value, &mut |_, _| {
        Err(failure("a data value cannot carry a resource"))
    })
}

/// Lower resources through an issuer-checked callback while reusing the ordinary data codec.
pub(super) fn lower_with(
    ty: Type,
    value: &Value,
    resource: &mut impl FnMut(ResourceType, &Value) -> Result<Val>,
) -> Result<Val> {
    let invalid = || failure("portable value differs from the component ABI type");
    Ok(match ty {
        Type::Bool => Val::Bool(value.as_bool().ok_or_else(invalid)?),
        Type::S8 => Val::S8(
            value
                .as_i64()
                .and_then(|v| v.try_into().ok())
                .ok_or_else(invalid)?,
        ),
        Type::S16 => Val::S16(
            value
                .as_i64()
                .and_then(|v| v.try_into().ok())
                .ok_or_else(invalid)?,
        ),
        Type::S32 => Val::S32(
            value
                .as_i64()
                .and_then(|v| v.try_into().ok())
                .ok_or_else(invalid)?,
        ),
        Type::U8 => Val::U8(
            value
                .as_u64()
                .and_then(|v| v.try_into().ok())
                .ok_or_else(invalid)?,
        ),
        Type::U16 => Val::U16(
            value
                .as_u64()
                .and_then(|v| v.try_into().ok())
                .ok_or_else(invalid)?,
        ),
        Type::U32 => Val::U32(
            value
                .as_u64()
                .and_then(|v| v.try_into().ok())
                .ok_or_else(invalid)?,
        ),
        // The packet wire spells 64-bit integers as the canonical decimal strings the contract
        // checked, so no JavaScript host ever parses an inexact number.
        Type::S64 => Val::S64(wide(value).ok_or_else(invalid)?),
        Type::U64 => Val::U64(wide(value).ok_or_else(invalid)?),
        Type::Float32 => Val::Float32(value.as_f64().ok_or_else(invalid)? as f32),
        Type::Float64 => Val::Float64(value.as_f64().ok_or_else(invalid)?),
        Type::Char => Val::Char(
            value
                .as_str()
                .and_then(|v| v.chars().next())
                .ok_or_else(invalid)?,
        ),
        Type::String => Val::String(value.as_str().ok_or_else(invalid)?.into()),
        Type::List(list) => Val::List(
            value
                .as_array()
                .ok_or_else(invalid)?
                .iter()
                .map(|value| lower_with(list.ty(), value, resource))
                .collect::<Result<_>>()?,
        ),
        Type::Record(record) => Val::Record(
            record
                .fields()
                .map(|field| {
                    Ok((
                        field.name.into(),
                        lower_with(field.ty, &value[javascript_name(field.name)], resource)?,
                    ))
                })
                .collect::<Result<_>>()?,
        ),
        Type::Tuple(tuple) => Val::Tuple(
            tuple
                .types()
                .zip(value.as_array().ok_or_else(invalid)?)
                .map(|(ty, value)| lower_with(ty, value, resource))
                .collect::<Result<_>>()?,
        ),
        Type::Option(option) => Val::Option(if value.is_null() {
            None
        } else {
            Some(Box::new(lower_with(option.ty(), value, resource)?))
        }),
        Type::Variant(variant) => {
            let tag = value["tag"].as_str().ok_or_else(invalid)?;
            let case = variant
                .cases()
                .find(|case| case.name == tag)
                .ok_or_else(invalid)?;
            Val::Variant(
                tag.into(),
                case.ty
                    .map(|ty| lower_with(ty, &value["val"], resource).map(Box::new))
                    .transpose()?,
            )
        }
        Type::Result(result) => {
            let mut payload = |ty: Option<Type>| {
                ty.map(|ty| lower_with(ty, &value["val"], resource).map(Box::new))
                    .transpose()
            };
            Val::Result(match value["tag"].as_str() {
                Some("ok") => Ok(payload(result.ok())?),
                Some("err") => Err(payload(result.err())?),
                _ => return Err(invalid()),
            })
        }
        Type::Enum(_) => Val::Enum(value.as_str().ok_or_else(invalid)?.into()),
        Type::Flags(flags) => Val::Flags(
            flags
                .names()
                .filter(|name| value[javascript_name(name)] == true)
                .map(str::to_owned)
                .collect(),
        ),
        Type::Own(ty) => resource(ty, value)?,
        _ => {
            return Err(failure(
                "component value requires an additional portable codec",
            ));
        }
    })
}

/// Lift actual ABI values before independently checking the declared WIT result.
pub(super) fn lift(value: Val) -> Result<Value> {
    lift_with(value, &mut |_| {
        Err(failure("a data result cannot carry a resource"))
    })
}

/// Lift every nested resource through the same issuer-owned table as whole resource results.
pub(super) fn lift_with(
    value: Val,
    resource: &mut impl FnMut(ResourceAny) -> Result<Value>,
) -> Result<Value> {
    Ok(match value {
        Val::Bool(value) => json!(value),
        Val::S8(value) => json!(value),
        Val::S16(value) => json!(value),
        Val::S32(value) => json!(value),
        Val::U8(value) => json!(value),
        Val::U16(value) => json!(value),
        Val::U32(value) => json!(value),
        Val::S64(value) => json!(value.to_string()),
        Val::U64(value) => json!(value.to_string()),
        Val::Float32(value) => float(value.into())?,
        Val::Float64(value) => float(value)?,
        Val::Char(value) => json!(value.to_string()),
        Val::String(value) => json!(value),
        Val::List(values) | Val::Tuple(values) => Value::Array(
            values
                .into_iter()
                .map(|value| lift_with(value, resource))
                .collect::<Result<_>>()?,
        ),
        Val::Record(fields) => Value::Object(
            fields
                .into_iter()
                .map(|(name, value)| Ok((javascript_name(&name), lift_with(value, resource)?)))
                .collect::<Result<_>>()?,
        ),
        Val::Option(value) => value
            .map(|value| lift_with(*value, resource))
            .transpose()?
            .unwrap_or(Value::Null),
        Val::Variant(tag, value) => tagged(tag, value, resource)?,
        Val::Result(result) => match result {
            Ok(value) => tagged("ok".into(), value, resource)?,
            Err(value) => tagged("err".into(), value, resource)?,
        },
        Val::Enum(value) => json!(value),
        Val::Flags(names) => Value::Object(
            names
                .into_iter()
                .map(|name| (javascript_name(&name), Value::Bool(true)))
                .collect(),
        ),
        Val::Resource(value) => resource(value)?,
        _ => {
            return Err(failure(
                "component result requires an additional portable codec",
            ));
        }
    })
}

/// Parse a wire-form integer whose canonical spelling callers already checked against WIT.
fn wide<T: std::str::FromStr>(value: &Value) -> Option<T> {
    value.as_str()?.parse().ok()
}

/// Refuse values JSON would silently change or replace with null.
fn float(value: f64) -> Result<Value> {
    if !value.is_finite() || (value == 0.0 && value.is_sign_negative()) {
        return Err(failure(
            "component float cannot be represented by the portable profile",
        ));
    }
    Ok(json!(value))
}

/// Preserve tagged result and variant payloads, including their absence.
fn tagged(
    tag: String,
    value: Option<Box<Val>>,
    resource: &mut impl FnMut(ResourceAny) -> Result<Value>,
) -> Result<Value> {
    Ok(match value {
        Some(value) => json!({"tag":tag,"val":lift_with(*value, resource)?}),
        None => json!({"tag":tag}),
    })
}

/// Inspect every resource before conversion, so a rejected sibling cannot strand later handles.
pub(super) fn resources(value: &Val, visit: &mut impl FnMut(ResourceAny)) {
    match value {
        Val::Resource(resource) => visit(*resource),
        Val::List(values) | Val::Tuple(values) => {
            for value in values {
                resources(value, visit);
            }
        }
        Val::Record(fields) => {
            for (_, value) in fields {
                resources(value, visit);
            }
        }
        Val::Option(Some(value)) | Val::Variant(_, Some(value)) => resources(value, visit),
        Val::Result(Ok(Some(value)) | Err(Some(value))) => resources(value, visit),
        _ => {}
    }
}
