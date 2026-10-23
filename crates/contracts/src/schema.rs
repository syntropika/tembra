mod length;
pub(crate) use length::{DecodePreflight, decode_preflight};

use serde_json::{Map, Value, json};

use crate::{ContractError, Result};

mod prepared;

pub use prepared::PreparedSchema;

const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;
const MAX_DEPTH: usize = 128;
const ANNOTATIONS: &[&str] = &[
    "title",
    "description",
    "default",
    "examples",
    "$comment",
    "readOnly",
    "writeOnly",
    "deprecated",
];
const KEYWORDS: &[&str] = &[
    "type",
    "const",
    "enum",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "anyOf",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "minLength",
    "maxLength",
    "minItems",
    "maxItems",
    "minProperties",
    "maxProperties",
    "uniqueItems",
];

fn error(reason: &str, path: &str, message: impl std::fmt::Display) -> ContractError {
    let location = path.trim_end_matches('/');
    ContractError::new(reason, format!("{path}: {message}")).with_path(if location.is_empty() {
        "/"
    } else {
        location
    })
}

pub(crate) fn portable(value: &Value, depth: usize) -> Result<()> {
    portable_walk::<false>(value, depth).map(|_| ())
}

// Classification is computed only after each node's ordinary portability checks.
// Every child is validated even after a numeric representation has been observed.
// The false specialization preserves the ordinary validator without classification work.
fn portable_walk<const CLASSIFY_NUMERIC: bool>(value: &Value, depth: usize) -> Result<bool> {
    if depth >= MAX_DEPTH && (value.is_array() || value.is_object()) {
        return Err(error("depth", "/", "JSON nesting limit exceeded"));
    }
    let mut numeric = false;
    match value {
        Value::Number(number) => {
            let mathematical = number
                .as_f64()
                .ok_or_else(|| error("number", "/", "number cannot be represented as binary64"))?;
            if !mathematical.is_finite()
                || (mathematical.fract() == 0.0 && mathematical.abs() > MAX_SAFE_INTEGER)
            {
                return Err(error(
                    "number",
                    "/",
                    "number is non-finite or outside the safe integral range",
                ));
            }
            if CLASSIFY_NUMERIC {
                numeric = !number.is_i64() && !number.is_u64();
            }
        }
        Value::Array(values) => {
            for value in values {
                numeric |= portable_walk::<CLASSIFY_NUMERIC>(value, depth + 1)?;
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                numeric |= portable_walk::<CLASSIFY_NUMERIC>(value, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(numeric)
}

/// Serialize strict portable JSON using RFC 8785, rejecting unsafe numbers.
///
/// # Errors
/// Rejects unsafe integral numbers, excessive nesting, or unsupported serialization.
/// The borrowed value is never modified.
pub fn canonical_json(value: &Value) -> Result<String> {
    portable(value, 0)?;
    serde_jcs::to_string(value).map_err(|error| ContractError::new("canonical", error.to_string()))
}

/// Compile the portable subset, inlining only acyclic local definition references.
///
/// # Errors
/// Rejects unsupported keywords/dialects, invalid schemas/defaults, unresolved or
/// recursive references, and exhausted expansion budgets. Source data is unchanged.
pub fn normalize_schema(schema: &Value) -> Result<Value> {
    normalize_schema_with_limits(schema, crate::source::CompileLimits::default())
}

/// Compile a source schema with caller-selected expansion depth and node budgets.
///
/// # Errors
/// Returns the same profile errors as [`normalize_schema`], including `schema_budget`.
pub fn normalize_schema_with_limits(
    schema: &Value,
    limits: crate::source::CompileLimits,
) -> Result<Value> {
    portable(schema, 0)?;
    let (expanded, definitions) = crate::source::resolve(schema, limits)?;
    for (path, definition) in definitions {
        compile(&definition, &format!("{path}/"), 0)?;
    }
    let mut normalized = compile(&expanded, "/", 0)?;
    crate::json::normalize_numbers(&mut normalized);
    Ok(normalized)
}

fn compile(schema: &Value, path: &str, depth: usize) -> Result<Value> {
    if depth > MAX_DEPTH {
        return Err(error("depth", path, "schema nesting limit exceeded"));
    }
    match schema {
        Value::Bool(true) => return Ok(json!({})),
        Value::Bool(false) => return Ok(Value::Bool(false)),
        _ => {}
    }
    let source = schema
        .as_object()
        .ok_or_else(|| error("schema", path, "schema must be an object or boolean"))?;
    let mut object = source.clone();
    if let Some(dialect) = object.remove("$schema")
        && dialect.as_str() != Some("https://json-schema.org/draft/2020-12/schema")
    {
        return Err(error(
            "dialect",
            path,
            "only JSON Schema 2020-12 is supported",
        ));
    }
    for key in object.keys() {
        if !KEYWORDS.contains(&key.as_str()) && !ANNOTATIONS.contains(&key.as_str()) {
            return Err(error(
                "unsupported_schema",
                &format!("{path}{key}"),
                "unsupported schema keyword",
            ));
        }
    }
    if let Some(types) = object.get("type").and_then(Value::as_array).cloned() {
        if types.is_empty() || object.contains_key("anyOf") {
            return Err(error("schema", path, "empty or ambiguous type union"));
        }
        let mut seen = std::collections::BTreeSet::new();
        let mut branches = types
            .into_iter()
            .map(|kind| {
                let name = kind
                    .as_str()
                    .ok_or_else(|| error("schema", path, "type entries must be strings"))?;
                if !seen.insert(name.to_owned()) {
                    return Err(error("schema", path, "duplicate union type"));
                }
                Ok(json!({"type": name}))
            })
            .collect::<Result<Vec<_>>>()?;
        // Type-specific cleaning belongs in the corresponding branch. Leaving it
        // beside anyOf would clean once for the branch and again for its parent.
        for (kind, keywords) in [
            (
                "object",
                &[
                    "properties",
                    "required",
                    "additionalProperties",
                    "minProperties",
                    "maxProperties",
                ][..],
            ),
            (
                "array",
                &["items", "minItems", "maxItems", "uniqueItems"][..],
            ),
        ] {
            if let Some(branch) = branches.iter_mut().find(|branch| branch["type"] == kind) {
                for keyword in keywords {
                    if let Some(value) = object.remove(*keyword) {
                        branch
                            .as_object_mut()
                            .expect("type branch is an object")
                            .insert((*keyword).into(), value);
                    }
                }
            }
        }
        object.remove("type");
        object.insert("anyOf".into(), Value::Array(branches));
    }
    if let Some(kind) = object.get("type")
        && !kind.as_str().is_some_and(|kind| {
            [
                "null", "boolean", "number", "integer", "string", "array", "object",
            ]
            .contains(&kind)
        })
    {
        return Err(error("schema", &format!("{path}type"), "unknown JSON type"));
    }
    for name in ["title", "description", "$comment"] {
        if object.get(name).is_some_and(|value| !value.is_string()) {
            return Err(error("schema", path, format!("{name} must be a string")));
        }
    }
    for name in [
        "readOnly",
        "writeOnly",
        "deprecated",
        "uniqueItems",
        "additionalProperties",
    ] {
        if object.get(name).is_some_and(|value| !value.is_boolean()) {
            return Err(error("schema", path, format!("{name} must be boolean")));
        }
    }
    if object
        .get("examples")
        .is_some_and(|value| !value.is_array())
    {
        return Err(error("schema", path, "examples must be an array"));
    }
    for name in ["minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum"] {
        if object.get(name).is_some_and(|value| !value.is_number()) {
            return Err(error("schema", path, format!("{name} must be numeric")));
        }
    }
    for name in [
        "minLength",
        "maxLength",
        "minItems",
        "maxItems",
        "minProperties",
        "maxProperties",
    ] {
        if object.get(name).is_some_and(|value| {
            !value
                .as_f64()
                .is_some_and(|number| number >= 0.0 && number.fract() == 0.0)
        }) {
            return Err(error(
                "schema",
                path,
                format!("{name} must be a nonnegative integer"),
            ));
        }
        if let Some(number) = object.get(name).and_then(Value::as_f64) {
            object.insert(name.into(), json!(number as u64));
        }
    }
    if let Some(properties) = object.get_mut("properties") {
        let properties = properties
            .as_object_mut()
            .ok_or_else(|| error("schema", path, "properties must be an object"))?;
        for (name, value) in properties {
            let name = name.replace('~', "~0").replace('/', "~1");
            *value = compile(value, &format!("{path}properties/{name}/"), depth + 1)?;
        }
    }
    if let Some(required) = object.get("required") {
        let keys = required
            .as_array()
            .ok_or_else(|| error("schema", path, "required must be an array"))?;
        let properties = object.get("properties").and_then(Value::as_object);
        let mut seen = std::collections::BTreeSet::new();
        for key in keys {
            let key = key
                .as_str()
                .ok_or_else(|| error("schema", path, "required names must be strings"))?;
            if !seen.insert(key) || !properties.is_some_and(|values| values.contains_key(key)) {
                return Err(error(
                    "schema",
                    path,
                    "required names must be unique declared properties",
                ));
            }
        }
    }
    if let Some(items) = object.get_mut("items") {
        *items = compile(items, &format!("{path}items/"), depth + 1)?;
    } else if object.get("type").and_then(Value::as_str) == Some("array") {
        object.insert("items".into(), json!({}));
    }
    if let Some(branches) = object.get_mut("anyOf") {
        let branches = branches
            .as_array_mut()
            .filter(|branches| !branches.is_empty())
            .ok_or_else(|| error("schema", path, "anyOf must be a nonempty array"))?;
        for (index, branch) in branches.iter_mut().enumerate() {
            *branch = compile(branch, &format!("{path}anyOf/{index}/"), depth + 1)?;
        }
    }
    if let Some(values) = object.get("enum") {
        let values = values
            .as_array()
            .filter(|values| !values.is_empty())
            .ok_or_else(|| error("schema", path, "enum must be a nonempty array"))?;
        let mut seen = std::collections::BTreeSet::new();
        for value in values {
            if !seen.insert(canonical_json(value)?) {
                return Err(error("schema", path, "enum contains a duplicate value"));
            }
        }
    }
    let normalized = Value::Object(object);
    if let Some(default) = normalized.get("default") {
        validate_inner(&normalized, default, path, depth + 1).map_err(|cause| {
            error(
                "schema",
                &format!("{path}default"),
                format!("default does not satisfy its schema: {cause}"),
            )
        })?;
    }
    Ok(normalized)
}

/// Validate values without applying defaults, cleaning fields, or coercing types.
///
/// # Errors
/// Returns a schema/profile error or `validation` when the value does not conform.
/// Both borrowed inputs remain unchanged on success and failure.
pub fn validate(schema: &Value, value: &Value) -> Result<()> {
    portable(value, 0)?;
    validate_inner(&normalize_schema(schema)?, value, "/", 0)
}

fn validate_inner(schema: &Value, value: &Value, path: &str, depth: usize) -> Result<()> {
    if depth > MAX_DEPTH {
        return Err(error("depth", path, "validation nesting limit exceeded"));
    }
    if schema == &Value::Bool(false) {
        return Err(error(
            "validation",
            path,
            "false schema rejects every value",
        ));
    }
    if let Some(kind) = schema.get("type").and_then(Value::as_str) {
        let matches = match kind {
            "null" => value.is_null(),
            "boolean" => value.is_boolean(),
            "string" => value.is_string(),
            "number" => value.is_number(),
            "integer" => value.as_f64().is_some_and(|value| value.fract() == 0.0),
            "array" => value.is_array(),
            "object" => value.is_object(),
            _ => false,
        };
        if !matches {
            return Err(error("validation", path, format!("expected {kind}")));
        }
    }
    if let Some(expected) = schema.get("const")
        && canonical_json(expected)? != canonical_json(value)?
    {
        return Err(error("validation", path, "value differs from const"));
    }
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        let candidate = canonical_json(value)?;
        let mut found = false;
        for expected in values {
            found |= canonical_json(expected)? == candidate;
        }
        if !found {
            return Err(error("validation", path, "value is not in enum"));
        }
    }
    if let Some(branches) = schema.get("anyOf").and_then(Value::as_array)
        && !branches
            .iter()
            .any(|branch| validate_inner(branch, value, path, depth + 1).is_ok())
    {
        return Err(error("validation", path, "value matches no anyOf branch"));
    }
    if let Some(number) = value.as_f64() {
        for (name, passes) in [
            ("minimum", 0),
            ("maximum", 1),
            ("exclusiveMinimum", 2),
            ("exclusiveMaximum", 3),
        ] {
            if let Some(bound) = schema.get(name).and_then(Value::as_f64) {
                let valid = match passes {
                    0 => number >= bound,
                    1 => number <= bound,
                    2 => number > bound,
                    _ => number < bound,
                };
                if !valid {
                    return Err(error("validation", path, format!("value violates {name}")));
                }
            }
        }
    }
    if let Some(text) = value.as_str() {
        check_length(schema, text.chars().count(), "minLength", "maxLength", path)?;
    }
    if let Some(values) = value.as_array() {
        check_length(schema, values.len(), "minItems", "maxItems", path)?;
        if schema.get("uniqueItems") == Some(&Value::Bool(true)) {
            let mut seen = std::collections::BTreeSet::new();
            for value in values {
                if !seen.insert(canonical_json(value)?) {
                    return Err(error("validation", path, "array values are not unique"));
                }
            }
        }
        if let Some(items) = schema.get("items") {
            for (index, value) in values.iter().enumerate() {
                validate_inner(items, value, &format!("{path}{index}/"), depth + 1)?;
            }
        }
    }
    if let Some(values) = value.as_object() {
        check_length(schema, values.len(), "minProperties", "maxProperties", path)?;
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for key in required {
                if let Some(key) = key.as_str()
                    && !values.contains_key(key)
                {
                    return Err(error(
                        "validation",
                        path,
                        format!("missing required property {key}"),
                    ));
                }
            }
        }
        let properties = schema.get("properties").and_then(Value::as_object);
        for (name, value) in values {
            if let Some(property) = properties.and_then(|properties| properties.get(name)) {
                validate_inner(property, value, &format!("{path}{name}/"), depth + 1)?;
            } else if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
                return Err(error(
                    "validation",
                    path,
                    format!("unknown property {name}"),
                ));
            }
        }
    }
    Ok(())
}

fn check_length(
    schema: &Value,
    length: usize,
    minimum: &str,
    maximum: &str,
    path: &str,
) -> Result<()> {
    if schema
        .get(minimum)
        .and_then(Value::as_u64)
        .is_some_and(|bound| (length as u64) < bound)
        || schema
            .get(maximum)
            .and_then(Value::as_u64)
            .is_some_and(|bound| length as u64 > bound)
    {
        return Err(error(
            "validation",
            path,
            format!("length violates {minimum}/{maximum}"),
        ));
    }
    Ok(())
}

/// Clone and clean arguments, without acquisition null fallback or default insertion.
///
/// This operation does not validate the returned argument. Dispatch cleans through
/// each contract and validates that contract's cleaned candidate before proceeding.
///
/// # Errors
/// Rejects invalid/unsupported schemas, nonportable data and ambiguous union cleaning.
/// Neither borrowed input is modified.
pub fn clean_arguments(schema: &Value, value: &Value) -> Result<Value> {
    portable(value, 0)?;
    let schema = normalize_schema(schema)?;
    check_normalization(&schema)?;
    transform(&schema, value, false, false, 0)
}

/// Clone, default, clean, then validate acquisition input. Top-level null means `{}`.
///
/// # Guarantees
/// A successful value conforms to the schema. Neither input is changed, including
/// on failure. Nested null is preserved; defaults fill absent properties only.
///
/// # Errors
/// Rejects invalid schemas, ambiguous normalization, nonportable values, or a
/// normalized candidate that does not validate. No partial candidate escapes.
///
/// ```
/// use serde_json::json;
/// use tembra_contracts::normalize_input;
/// let schema = json!({"type":"object", "properties":{
///     "count":{"type":"integer", "default":0}
/// }});
/// assert_eq!(normalize_input(&schema, &json!(null))?, json!({"count":0}));
/// let invalid = json!({"count":"wrong", "extra":true});
/// let before = invalid.clone();
/// assert!(normalize_input(&schema, &invalid).is_err());
/// assert_eq!(invalid, before);
/// # Ok::<(), tembra_contracts::ContractError>(())
/// ```
pub fn normalize_input(schema: &Value, value: &Value) -> Result<Value> {
    portable(value, 0)?;
    let schema = normalize_schema(schema)?;
    check_normalization(&schema)?;
    let fallback = json!({});
    let value = if value.is_null() { &fallback } else { value };
    let normalized = transform(&schema, value, true, false, 0)?;
    validate_inner(&schema, &normalized, "/", 0)?;
    Ok(normalized)
}

pub(crate) fn identity_value(schema: &Value, value: &Value) -> Result<Value> {
    transform(schema, value, false, true, 0)
}

fn transform(
    schema: &Value,
    value: &Value,
    defaults: bool,
    identity: bool,
    depth: usize,
) -> Result<Value> {
    if depth > MAX_DEPTH {
        return Err(error("depth", "/", "normalization nesting limit exceeded"));
    }
    let mut result = value.clone();
    if let Some(branches) = schema.get("anyOf").and_then(Value::as_array) {
        let branch = if let Some((property, tags)) = discriminated_branches(branches) {
            // A nullable outer union can apply this branch to null. Nonobjects have no
            // object cleaning to perform and remain available for final validation.
            if !result.is_object() {
                None
            } else {
                let tag = result.get(property).and_then(Value::as_str);
                let index = tags
                    .iter()
                    .position(|candidate| Some(*candidate) == tag)
                    .ok_or_else(|| {
                        error("validation", "/", "missing or unknown union discriminant")
                    })?;
                branches.get(index)
            }
        } else {
            branches.get(nullable_branch(branches).unwrap_or(0))
        };
        if let Some(branch) = branch {
            result = transform(branch, &result, defaults, identity, depth + 1)?;
        }
    }
    if let Some(values) = result.as_array_mut()
        && let Some(items) = schema.get("items")
    {
        for value in values {
            *value = transform(items, value, defaults, identity, depth + 1)?;
        }
    }
    // An unconstrained schema does not imply Type.Object({}): it preserves values.
    let object_schema = schema.get("type").and_then(Value::as_str) == Some("object")
        || schema.get("properties").is_some()
        || schema.get("additionalProperties").is_some();
    if object_schema && let Some(values) = result.as_object_mut() {
        let empty = Map::new();
        let properties = schema
            .get("properties")
            .and_then(Value::as_object)
            .unwrap_or(&empty);
        if defaults {
            for (name, property) in properties {
                if !values.contains_key(name)
                    && let Some(default) = property.get("default")
                {
                    values.insert(name.clone(), default.clone());
                }
            }
        }
        if !identity && schema.get("additionalProperties") != Some(&Value::Bool(true)) {
            values.retain(|name, _| properties.contains_key(name));
        }
        for (name, property) in properties {
            let Some(value) = values.get(name) else {
                continue;
            };
            if identity
                && property.get("default").is_some_and(|default| {
                    canonical_json(default).ok() == canonical_json(value).ok()
                })
            {
                values.remove(name);
            } else {
                let next = transform(property, value, defaults, identity, depth + 1)?;
                values.insert(name.clone(), next);
            }
        }
    }
    Ok(result)
}

/// Accept branch-independent policies or object unions selected by a unique required literal tag.
pub(crate) fn check_normalization(schema: &Value) -> Result<()> {
    normalization_signature(schema).map(|_| ())
}

pub(crate) fn normalization_signature(schema: &Value) -> Result<Value> {
    normalization_signature_at(schema, "/")
}

// A plain null alternative is inert for all transformations. Applying the other
// branch to a null value leaves it null, without branch-dependent coercion.
fn nullable_branch(branches: &[Value]) -> Option<usize> {
    if branches.len() != 2 {
        return None;
    }
    branches
        .iter()
        .position(|branch| {
            branch.as_object().is_some_and(|object| {
                object.len() == 1 && object.get("type").and_then(Value::as_str) == Some("null")
            })
        })
        .map(|index| 1 - index)
}

// Required, distinct string constants prove branch selection before cleaning/defaults.
// The discriminant cannot have a default: identity normalization must retain its value.
fn discriminated_branches(branches: &[Value]) -> Option<(&str, Vec<&str>)> {
    let first = branches.first()?.get("properties")?.as_object()?;
    for property in first.keys() {
        let mut tags = Vec::with_capacity(branches.len());
        for branch in branches {
            if branch.get("type").and_then(Value::as_str) != Some("object")
                || !branch
                    .get("required")
                    .and_then(Value::as_array)
                    .is_some_and(|required| {
                        required.iter().any(|name| name.as_str() == Some(property))
                    })
            {
                break;
            }
            let Some(schema) = branch
                .get("properties")
                .and_then(|properties| properties.get(property))
            else {
                break;
            };
            let Some(tag) = schema.get("const").and_then(Value::as_str) else {
                break;
            };
            if schema.get("default").is_some() || tags.contains(&tag) {
                break;
            }
            tags.push(tag);
        }
        if tags.len() == branches.len() {
            return Some((property, tags));
        }
    }
    None
}

fn normalization_signature_at(schema: &Value, path: &str) -> Result<Value> {
    let mut signature = Map::new();
    if let Some(default) = schema.get("default") {
        signature.insert("default".into(), default.clone());
    }
    if schema.get("type").and_then(Value::as_str) == Some("object")
        || schema.get("properties").is_some()
        || schema.get("additionalProperties").is_some()
    {
        signature.insert(
            "object".into(),
            Value::Bool(schema.get("additionalProperties") == Some(&Value::Bool(true))),
        );
        let mut properties = Map::new();
        if let Some(values) = schema.get("properties").and_then(Value::as_object) {
            for (name, property) in values {
                let name_path = name.replace('~', "~0").replace('/', "~1");
                properties.insert(
                    name.clone(),
                    normalization_signature_at(
                        property,
                        &format!("{path}properties/{name_path}/"),
                    )?,
                );
            }
        }
        signature.insert("properties".into(), Value::Object(properties));
    }
    if let Some(items) = schema.get("items") {
        let inner = normalization_signature_at(items, &format!("{path}items/"))?;
        if inner.as_object().is_some_and(|object| !object.is_empty()) {
            signature.insert("items".into(), inner);
        }
    }
    if let Some(branches) = schema.get("anyOf").and_then(Value::as_array) {
        let selected = nullable_branch(branches);
        let mut policies = Vec::new();
        for (index, branch) in branches.iter().enumerate() {
            if selected.is_some_and(|selected| index != selected) {
                continue;
            }
            policies.push(normalization_signature_at(
                branch,
                &format!("{path}anyOf/{index}/"),
            )?);
        }
        let same_policy = policies.windows(2).all(|pair| pair[0] == pair[1]);
        let policy = if same_policy {
            policies.into_iter().next().unwrap_or_else(|| json!({}))
        } else if let Some((property, tags)) = discriminated_branches(branches) {
            let alternatives = tags
                .into_iter()
                .zip(policies)
                .map(|(tag, policy)| (tag.to_owned(), policy))
                .collect::<Map<_, _>>();
            json!({"tagged": {"property": property, "alternatives": alternatives}})
        } else {
            return Err(error(
                "ambiguous_normalization",
                &format!("{path}anyOf"),
                "union branches need identical cleaning/default policies or distinct required string tags",
            ));
        };
        if policy.as_object().is_some_and(|object| !object.is_empty()) {
            if signature.contains_key("object") || signature.contains_key("items") {
                return Err(error(
                    "ambiguous_normalization",
                    &format!("{path}anyOf"),
                    "sibling and union cleaning/default policies cannot be combined",
                ));
            }
            signature.insert("union".into(), policy);
        }
    }
    Ok(Value::Object(signature))
}

// Frozen baseline portability oracle; independent of the optimized walk.
#[cfg(test)]
pub(crate) fn frozen_portable(value: &Value, depth: usize) -> Result<()> {
    if depth >= MAX_DEPTH && (value.is_array() || value.is_object()) {
        return Err(error("depth", "/", "JSON nesting limit exceeded"));
    }
    match value {
        Value::Number(number) => {
            let number = number
                .as_f64()
                .ok_or_else(|| error("number", "/", "number cannot be represented as binary64"))?;
            if !number.is_finite() || (number.fract() == 0.0 && number.abs() > MAX_SAFE_INTEGER) {
                return Err(error(
                    "number",
                    "/",
                    "number is non-finite or outside the safe integral range",
                ));
            }
        }
        Value::Array(values) => {
            for value in values {
                frozen_portable(value, depth + 1)?;
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                frozen_portable(value, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}
