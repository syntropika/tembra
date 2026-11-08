//! Validate the deliberately bounded JSON value profile used by interpreted callbacks.
use crate::Error;
use serde_json::Value;
use wit_parser::{Function, Resolve, Type, TypeDefKind, WorldId, WorldItem};

/// Find the complete interface identity; method spelling alone never selects a contract.
fn function<'a>(
    resolve: &'a Resolve,
    world: WorldId,
    interface: &str,
    method: &str,
    exports: bool,
) -> Result<&'a Function, Error> {
    let items = if exports {
        &resolve.worlds[world].exports
    } else {
        &resolve.worlds[world].imports
    };
    items
        .iter()
        .find_map(|(key, item)| {
            let WorldItem::Interface { id, .. } = item else {
                return None;
            };
            let selected = if exports {
                resolve.id_of(*id).as_deref() == Some(interface)
            } else {
                resolve.name_world_key(key) == interface
            };
            selected
                .then(|| resolve.interfaces[*id].functions.get(method))
                .flatten()
        })
        .ok_or_else(|| Error(format!("unknown WIT export {interface}#{method}")))
}

/// Require every exported signature to use types supported by this portable profile.
pub(crate) fn profile(resolve: &Resolve, world: WorldId) -> Result<(), Error> {
    for item in resolve.worlds[world].exports.values() {
        let WorldItem::Interface { id, .. } = item else {
            continue;
        };
        for function in resolve.interfaces[*id].functions.values() {
            for parameter in &function.params {
                supported(resolve, parameter.ty, 0)?;
            }
            if let Some(result) = function.result {
                supported(resolve, result, 0)?;
            }
        }
    }
    Ok(())
}

/// Check an Orb import without applying the JSON profile to standard WASI resource imports.
pub(crate) fn import_profile(
    resolve: &Resolve,
    world: WorldId,
    binding: &str,
) -> Result<(), Error> {
    for (key, item) in &resolve.worlds[world].imports {
        if resolve.name_world_key(key) != binding {
            continue;
        }
        let WorldItem::Interface { id, .. } = item else {
            return Err(Error("Orb import must be an interface".into()));
        };
        for function in resolve.interfaces[*id].functions.values() {
            for parameter in &function.params {
                supported(resolve, parameter.ty, 0)?;
            }
            if let Some(result) = function.result {
                supported(resolve, result, 0)?;
            }
        }
        return Ok(());
    }
    Err(Error(format!("unknown WIT import {binding}")))
}

/// Refuse lossy integer, native handle and asynchronous container coercions before execution.
fn supported(resolve: &Resolve, ty: Type, depth: usize) -> Result<(), Error> {
    if depth > 64 {
        return Err(Error(
            "WIT type nesting exceeds the value profile limit".into(),
        ));
    }
    match ty {
        Type::U64 | Type::S64 | Type::ErrorContext => {
            return Err(Error(
                "64-bit integers and error contexts require a dedicated value codec".into(),
            ));
        }
        Type::Id(id) => match &resolve.types[id].kind {
            TypeDefKind::Option(ty) => {
                let mut inner = *ty;
                while let Type::Id(id) = inner {
                    match resolve.types[id].kind {
                        TypeDefKind::Type(next) => inner = next,
                        TypeDefKind::Option(_) => {
                            return Err(Error(
                                "nested options require a tagged option codec".into(),
                            ));
                        }
                        _ => break,
                    }
                }
                supported(resolve, *ty, depth + 1)?;
            }
            TypeDefKind::Type(ty) | TypeDefKind::List(ty) => supported(resolve, *ty, depth + 1)?,
            TypeDefKind::Record(record) => {
                for field in &record.fields {
                    supported(resolve, field.ty, depth + 1)?;
                }
            }
            TypeDefKind::Tuple(tuple) => {
                for ty in &tuple.types {
                    supported(resolve, *ty, depth + 1)?;
                }
            }
            TypeDefKind::Variant(variant) => {
                for case in &variant.cases {
                    if let Some(ty) = case.ty {
                        supported(resolve, ty, depth + 1)?;
                    }
                }
            }
            TypeDefKind::Result(result) => {
                for ty in [result.ok, result.err].into_iter().flatten() {
                    supported(resolve, ty, depth + 1)?;
                }
            }
            TypeDefKind::Flags(_) | TypeDefKind::Enum(_) => {}
            other => {
                return Err(Error(format!(
                    "{} requires a dedicated interpreter value adapter",
                    other.as_str()
                )));
            }
        },
        _ => {}
    }
    Ok(())
}

/// Validate tuple length and each named parameter before invoking user code.
pub(crate) fn arguments(
    resolve: &Resolve,
    world: WorldId,
    interface: &str,
    method: &str,
    arguments: &Value,
    exports: bool,
) -> Result<(), Error> {
    let signature = function(resolve, world, interface, method, exports)?;
    let values = arguments
        .as_array()
        .ok_or_else(|| Error("WIT arguments must be an array".into()))?;
    if values.len() != signature.params.len() {
        return Err(Error(format!(
            "{method}: expected {} arguments",
            signature.params.len()
        )));
    }
    for (parameter, value) in signature.params.iter().zip(values) {
        validate(resolve, parameter.ty, value, 0)
            .map_err(|error| Error(format!("{method} argument {}: {error}", parameter.name)))?;
    }
    Ok(())
}

/// Validate the single WIT result or the absence of a result.
pub(crate) fn result(
    resolve: &Resolve,
    world: WorldId,
    interface: &str,
    method: &str,
    value: &Value,
    exports: bool,
) -> Result<(), Error> {
    let signature = function(resolve, world, interface, method, exports)?;
    match signature.result {
        Some(ty) => validate(resolve, ty, value, 0),
        None if value.is_null() => Ok(()),
        None => Err(Error("WIT function has no result".into())),
    }
}

/// Use the default JavaScript field spelling emitted by the official WIT bindings.
pub fn javascript_name(name: &str) -> String {
    let mut parts = name.split('-');
    let mut name = parts.next().unwrap_or_default().to_owned();
    for part in parts {
        let mut chars = part.chars();
        if let Some(first) = chars.next() {
            name.extend(first.to_uppercase());
        }
        name.extend(chars);
    }
    name
}

/// Check values recursively without silently truncating fields or numeric widths.
fn validate(resolve: &Resolve, ty: Type, value: &Value, depth: usize) -> Result<(), Error> {
    if depth > 64 {
        return Err(Error("WIT value nesting exceeds its limit".into()));
    }
    let valid = match ty {
        Type::Bool => value.is_boolean(),
        Type::U8 => value
            .as_u64()
            .is_some_and(|value| u8::try_from(value).is_ok()),
        Type::U16 => value
            .as_u64()
            .is_some_and(|value| u16::try_from(value).is_ok()),
        Type::U32 => value
            .as_u64()
            .is_some_and(|value| u32::try_from(value).is_ok()),
        Type::S8 => value
            .as_i64()
            .is_some_and(|value| i8::try_from(value).is_ok()),
        Type::S16 => value
            .as_i64()
            .is_some_and(|value| i16::try_from(value).is_ok()),
        Type::S32 => value
            .as_i64()
            .is_some_and(|value| i32::try_from(value).is_ok()),
        Type::F32 => value
            .as_f64()
            .is_some_and(|value| finite(value) && (value as f32).is_finite()),
        Type::F64 => value.as_f64().is_some_and(finite),
        Type::String => value.is_string(),
        Type::Char => value
            .as_str()
            .is_some_and(|value| value.chars().count() == 1),
        Type::Id(id) => match &resolve.types[id].kind {
            TypeDefKind::Type(ty) => return validate(resolve, *ty, value, depth + 1),
            TypeDefKind::List(ty) => {
                let values = value
                    .as_array()
                    .ok_or_else(|| Error("expected WIT list".into()))?;
                for value in values {
                    validate(resolve, *ty, value, depth + 1)?;
                }
                true
            }
            TypeDefKind::Record(record) => {
                let object = value
                    .as_object()
                    .ok_or_else(|| Error("expected WIT record".into()))?;
                if object.len() != record.fields.len() {
                    return Err(Error("record fields differ from WIT".into()));
                }
                for field in &record.fields {
                    let field_value = object
                        .get(&javascript_name(&field.name))
                        .ok_or_else(|| Error(format!("missing WIT field {}", field.name)))?;
                    validate(resolve, field.ty, field_value, depth + 1)?;
                }
                true
            }
            TypeDefKind::Tuple(tuple) => {
                let values = value
                    .as_array()
                    .filter(|values| values.len() == tuple.types.len())
                    .ok_or_else(|| Error("WIT tuple length differs".into()))?;
                for (ty, value) in tuple.types.iter().zip(values) {
                    validate(resolve, *ty, value, depth + 1)?;
                }
                true
            }
            TypeDefKind::Option(ty) => {
                if !value.is_null() {
                    validate(resolve, *ty, value, depth + 1)?;
                }
                true
            }
            TypeDefKind::Enum(enumeration) => value
                .as_str()
                .is_some_and(|value| enumeration.cases.iter().any(|case| case.name == value)),
            TypeDefKind::Flags(flags) => value.as_object().is_some_and(|object| {
                object.iter().all(|(key, value)| {
                    value.is_boolean()
                        && flags
                            .flags
                            .iter()
                            .any(|flag| javascript_name(&flag.name) == *key)
                })
            }),
            TypeDefKind::Variant(variant) => {
                let tag = value["tag"]
                    .as_str()
                    .ok_or_else(|| Error("variant needs a tag".into()))?;
                let case = variant
                    .cases
                    .iter()
                    .find(|case| case.name == tag)
                    .ok_or_else(|| Error("unknown WIT variant case".into()))?;
                tagged(resolve, value, case.ty, depth)?;
                true
            }
            TypeDefKind::Result(result) => {
                let ty = match value["tag"].as_str() {
                    Some("ok") => result.ok,
                    Some("err") => result.err,
                    _ => return Err(Error("result needs an ok or err tag".into())),
                };
                tagged(resolve, value, ty, depth)?;
                true
            }
            other => return Err(Error(format!("unsupported WIT value {}", other.as_str()))),
        },
        _ => return Err(Error("WIT type requires a dedicated value codec".into())),
    };
    if valid {
        Ok(())
    } else {
        Err(Error(format!("value does not match {ty:?}")))
    }
}

/// JSON cannot preserve negative zero between the two heaps.
fn finite(value: f64) -> bool {
    value.is_finite() && !(value == 0.0 && value.is_sign_negative())
}

/// Validate a tagged payload and refuse undeclared keys or missing payloads.
fn tagged(resolve: &Resolve, value: &Value, ty: Option<Type>, depth: usize) -> Result<(), Error> {
    let object = value
        .as_object()
        .ok_or_else(|| Error("expected tagged WIT object".into()))?;
    let expected = if ty.is_some() { 2 } else { 1 };
    if object.len() != expected {
        return Err(Error("tagged WIT fields differ".into()));
    }
    if let Some(ty) = ty {
        validate(
            resolve,
            ty,
            object
                .get("val")
                .ok_or_else(|| Error("missing WIT payload".into()))?,
            depth + 1,
        )?;
    }
    Ok(())
}
