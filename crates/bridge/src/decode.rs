//! Reject invalid wire identities before calling the core.

use super::*;

/// Map unknown callback error codes to the invalid category.
pub(super) fn parse_code(code: &str) -> Code {
    Code::parse(code).unwrap_or(Code::Invalid)
}

/// Decode facet contracts without retaining language values.
pub(super) fn parse_facet(f: &Value) -> Result<Facet> {
    let services = array(f, "services")?
        .iter()
        .map(|s| {
            Ok(Service {
                name: text(s, "name")?.into(),
                methods: strings(s, "methods")?.into_iter().collect(),
                lifetime: match s["lifetime"].as_str().unwrap_or("referenced") {
                    "host" => Lifetime::Host,
                    "referenced" => Lifetime::Referenced,
                    _ => return Err(Error::invalid(Reason::Lifetime, "unknown lifetime")),
                },
                linger_ms: optional_number(s, "linger")?.unwrap_or(0),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let requires = f
        .get("requires")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|r| {
            let mut methods = BTreeMap::new();
            if let Some(map) = r["services"].as_object() {
                for (key, value) in map {
                    methods.insert(
                        key.clone(),
                        value
                            .as_array()
                            .ok_or_else(|| {
                                Error::invalid(Reason::Requires, "methods must be an array")
                            })?
                            .iter()
                            .map(|v| {
                                v.as_str().map(String::from).ok_or_else(|| {
                                    Error::invalid(Reason::Requires, "method must be text")
                                })
                            })
                            .collect::<Result<BTreeSet<_>>>()?,
                    );
                }
            }
            Ok(Requirement {
                facet: text(r, "facet")?.into(),
                api_version: number(r, "apiVersion")?,
                methods,
                hooks: strings_optional(r, "hooks")?.into_iter().collect(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Facet {
        name: text(f, "name")?.into(),
        api_version: number(f, "apiVersion")?,
        services,
        requires,
        hooks: strings_optional(f, "hooks")?.into_iter().collect(),
    })
}

/// Decode the host, facet or object that owns a declaration.
pub(super) fn owner(v: &Value) -> Result<Owner> {
    match v["owner"]["kind"].as_str() {
        Some("host") => Ok(Owner::Host),
        Some("facet") => Ok(Owner::Facet(identity(&v["owner"], "id")?)),
        Some("object") => Ok(Owner::Object(identity(&v["owner"], "id")?)),
        _ => Err(Error::invalid(Reason::Owner, "invalid owner")),
    }
}

/// Require a text field in request metadata.
pub(super) fn text<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v[key]
        .as_str()
        .ok_or_else(|| Error::invalid(Reason::Metadata, format!("{key} must be text")))
}

/// Require an unsigned 32-bit metadata field.
pub(super) fn number(v: &Value, key: &str) -> Result<u32> {
    v[key]
        .as_u64()
        .and_then(|n| n.try_into().ok())
        .ok_or_else(|| Error::invalid(Reason::Metadata, format!("{key} must be a u32")))
}

/// Decode an optional unsigned field without accepting invalid values.
pub(super) fn optional_number(v: &Value, key: &str) -> Result<Option<u32>> {
    if v.get(key).is_none_or(Value::is_null) {
        Ok(None)
    } else {
        number(v, key).map(Some)
    }
}

/// Decode a wide revision or clock from text or an unsigned integer.
pub(super) fn wide(v: &Value, key: &str) -> Result<u64> {
    v[key]
        .as_str()
        .and_then(|s| s.parse().ok())
        .or_else(|| v[key].as_u64())
        .ok_or_else(|| {
            Error::invalid(
                Reason::Metadata,
                format!("{key} must be an unsigned revision or clock"),
            )
        })
}

/// Require an array field in request metadata.
pub(super) fn array<'a>(v: &'a Value, key: &str) -> Result<&'a Vec<Value>> {
    v[key]
        .as_array()
        .ok_or_else(|| Error::invalid(Reason::Metadata, format!("{key} must be an array")))
}

/// Require an array containing only text values.
pub(super) fn strings(v: &Value, key: &str) -> Result<Vec<String>> {
    array(v, key)?
        .iter()
        .map(|v| {
            v.as_str()
                .map(String::from)
                .ok_or_else(|| Error::invalid(Reason::Metadata, "expected text"))
        })
        .collect()
}

/// Use an empty list only when the text-array field is absent.
pub(super) fn strings_optional(v: &Value, key: &str) -> Result<Vec<String>> {
    if v.get(key).is_none() {
        Ok(Vec::new())
    } else {
        strings(v, key)
    }
}

/// Reject identities that JavaScript cannot represent exactly.
pub(super) fn identity_value(v: &Value) -> Result<Id> {
    v.as_u64()
        .filter(|id| *id <= MAX_ID)
        .ok_or_else(|| Error::invalid(Reason::Metadata, "identity must be a safe unsigned integer"))
}

/// Decode a required identity with the shared precision limit.
pub(super) fn identity(v: &Value, key: &str) -> Result<Id> {
    identity_value(&v[key])
}

/// Decode an optional identity with the same precision limit.
pub(super) fn optional_identity(v: &Value, key: &str) -> Result<Option<Id>> {
    if v.get(key).is_none_or(Value::is_null) {
        Ok(None)
    } else {
        identity(v, key).map(Some)
    }
}

/// Decode an identity array and reject unsafe values.
pub(super) fn numbers(v: &Value, key: &str) -> Result<Vec<Id>> {
    array(v, key)?.iter().map(identity_value).collect()
}

/// Use an empty list only when the identity-array field is absent.
pub(super) fn numbers_optional(v: &Value, key: &str) -> Result<Vec<Id>> {
    if v.get(key).is_none() {
        Ok(Vec::new())
    } else {
        numbers(v, key)
    }
}
