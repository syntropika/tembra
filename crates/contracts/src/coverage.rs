use serde_json::{Map, Value, json};

use crate::{
    Contract, ContractError, Member, Result, ValueSchema, canonical_json, clean_arguments,
    normalize_schema, validate,
};

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
const CONSTRAINTS: &[&str] = &[
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

fn mismatch(message: impl Into<String>) -> ContractError {
    ContractError::new("contract_mismatch", message)
}

/// Require unchanged identity defaults and cleaning behavior under one type name.
///
/// # Errors
/// Rejects invalid schemas and changed normalization recipes. Validation-only
/// changes are checked separately by the directional compatibility operations.
pub fn identity_compatible(first: &Value, second: &Value) -> Result<()> {
    let first = normalize_schema(first)?;
    let second = normalize_schema(second)?;
    if crate::schema::normalization_signature(&first)?
        == crate::schema::normalization_signature(&second)?
    {
        Ok(())
    } else {
        Err(mismatch(
            "identity defaults or cleaning changed; use a new contract name",
        ))
    }
}

/// Prove raw provider result/state values are valid for the consumer. Never cleans output.
///
/// This is a conservative structural check. Failure can mean that a safe inclusion
/// is outside the supported proof rules; it does not establish a counterexample.
///
/// # Errors
/// Rejects invalid schemas or unproved inclusion. Neither schema is modified.
pub fn output_fits(provider: &Value, consumer: &Value) -> Result<()> {
    let provider = normalize_schema(provider)?;
    let consumer = normalize_schema(consumer)?;
    if fits(&provider, &consumer, 0)? {
        Ok(())
    } else {
        Err(mismatch(
            "output/state schema inclusion could not be proved",
        ))
    }
}

/// Prove argument acceptance after consumer and provider cleaning, without defaults.
///
/// The domain is arguments valid under the consumer schema after its own cleaning.
/// Output/state compatibility must instead use [`output_fits`].
///
/// # Errors
/// Rejects invalid schemas, ambiguous normalization or an unproved projection relation.
pub fn input_compatible(consumer: &Value, provider: &Value) -> Result<()> {
    let consumer = normalize_schema(consumer)?;
    let provider = normalize_schema(provider)?;
    crate::schema::check_normalization(&consumer)?;
    crate::schema::check_normalization(&provider)?;
    if let Some(values) = finite(&consumer) {
        for value in values {
            if validate(&consumer, value).is_ok() {
                let sent = clean_arguments(&consumer, value)?;
                let received = clean_arguments(&provider, &sent)?;
                validate(&provider, &received).map_err(|_| {
                    mismatch("finite consumer argument is rejected after provider cleaning")
                })?;
            }
        }
        return Ok(());
    }
    let cleaned = projected(&consumer, &provider)?;
    if fits(&cleaned, &provider, 0)? {
        Ok(())
    } else {
        Err(mismatch(
            "cleaned argument schema inclusion could not be proved",
        ))
    }
}

impl Contract {
    /// Same-version direct binding. Other versions require explicit provider adapters.
    ///
    /// # Guarantees
    /// This operation only inspects immutable metadata. Failure cannot change either
    /// token. It never installs an adapter, activates an object, or dispatches a call.
    ///
    /// # Errors
    /// Rejects changed headers/placement/identity, missing members/variants, altered
    /// argument presence, nominal mismatches, or unproved directional schema relations.
    ///
    /// ```
    /// use std::collections::BTreeMap;
    /// use serde_json::json;
    /// use tembra_contracts::{Contract, Member, ValueSchema};
    /// let make = |result| Contract::local("example", 1, json!({}),
    ///     BTreeMap::from([("read".into(), Member::Method {
    ///         args: None, result: Some(ValueSchema::Portable(result))
    ///     })]));
    /// let consumer = make(json!({"type":"integer"}))?;
    /// let provider = make(json!({"type":"number"}))?;
    /// let before = consumer.fingerprint().to_owned();
    /// assert!(provider.covers(&consumer).is_err());
    /// assert_eq!(consumer.fingerprint(), before);
    /// consumer.covers(&consumer)?;
    /// # Ok::<(), tembra_contracts::ContractError>(())
    /// ```
    pub fn covers(&self, consumer: &Self) -> Result<()> {
        if self.name() != consumer.name()
            || self.kind() != consumer.kind()
            || self.version() != consumer.version()
        {
            return Err(mismatch(
                "direct binding requires equal type name, kind, and API version",
            ));
        }
        if self.placement() != consumer.placement() {
            return Err(mismatch("placement changed"));
        }
        identity_compatible(self.input(), consumer.input())?;
        input_compatible(consumer.input(), self.input())?;
        for (variant, wanted) in consumer.variants() {
            let offered = self
                .members(variant)
                .ok_or_else(|| mismatch(format!("missing variant {variant}")))?;
            for (name, wanted) in wanted {
                let offered = offered
                    .get(name)
                    .ok_or_else(|| mismatch(format!("missing member {variant}.{name}")))?;
                match (offered, wanted) {
                    (
                        Member::Method {
                            args: offered_args,
                            result: offered_result,
                        },
                        Member::Method {
                            args: wanted_args,
                            result: wanted_result,
                        },
                    ) => {
                        match (wanted_args, offered_args) {
                            (None, None) => {}
                            (Some(wanted), Some(offered)) => signature_input(wanted, offered)?,
                            _ => {
                                return Err(mismatch(format!(
                                    "argument presence changed for {name}"
                                )));
                            }
                        }
                        if let Some(wanted) = wanted_result {
                            let offered = offered_result.as_ref().ok_or_else(|| {
                                mismatch(format!("void provider for value consumer {name}"))
                            })?;
                            signature_output(offered, wanted)?;
                        }
                    }
                    (Member::State { schema: offered }, Member::State { schema: wanted }) => {
                        output_fits(offered, wanted)?
                    }
                    (Member::Opaque { id: offered }, Member::Opaque { id: wanted })
                        if offered == wanted => {}
                    _ => {
                        return Err(mismatch(format!(
                            "member kind or opaque identity changed for {name}"
                        )));
                    }
                }
            }
        }
        Ok(())
    }
}

fn signature_input(consumer: &ValueSchema, provider: &ValueSchema) -> Result<()> {
    match (consumer, provider) {
        (ValueSchema::Portable(consumer), ValueSchema::Portable(provider)) => {
            input_compatible(consumer, provider)
        }
        (ValueSchema::Native(consumer), ValueSchema::Native(provider)) if consumer == provider => {
            Ok(())
        }
        _ => Err(mismatch(
            "native/portable signature or nominal native identity mismatch",
        )),
    }
}

fn signature_output(provider: &ValueSchema, consumer: &ValueSchema) -> Result<()> {
    match (provider, consumer) {
        (ValueSchema::Portable(provider), ValueSchema::Portable(consumer)) => {
            output_fits(provider, consumer)
        }
        (ValueSchema::Native(provider), ValueSchema::Native(consumer)) if consumer == provider => {
            Ok(())
        }
        _ => Err(mismatch(
            "native/portable signature or nominal native identity mismatch",
        )),
    }
}

fn finite(schema: &Value) -> Option<Vec<&Value>> {
    schema.get("const").map(|value| vec![value]).or_else(|| {
        schema
            .get("enum")
            .and_then(Value::as_array)
            .map(|values| values.iter().collect())
    })
}

fn validity(schema: &Value) -> Value {
    match schema {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .filter(|(key, _)| !ANNOTATIONS.contains(&key.as_str()))
                .map(|(key, value)| {
                    let value = match key.as_str() {
                        "properties" => Value::Object(
                            value
                                .as_object()
                                .into_iter()
                                .flat_map(|values| values.iter())
                                .map(|(name, value)| (name.clone(), validity(value)))
                                .collect(),
                        ),
                        "items" => validity(value),
                        "anyOf" => Value::Array(
                            value
                                .as_array()
                                .into_iter()
                                .flatten()
                                .map(validity)
                                .collect(),
                        ),
                        _ => value.clone(),
                    };
                    (key.clone(), value)
                })
                .collect(),
        ),
        _ => schema.clone(),
    }
}

fn fits(sub: &Value, sup: &Value, depth: usize) -> Result<bool> {
    if depth > 128 {
        return Err(mismatch("coverage depth budget exceeded"));
    }
    if sub == &Value::Bool(false) || validity(sup).as_object().is_some_and(Map::is_empty) {
        return Ok(true);
    }
    if canonical_json(&validity(sub))? == canonical_json(&validity(sup))? {
        return Ok(true);
    }
    if let Some(values) = finite(sub) {
        for value in values {
            if validate(sub, value).is_ok() && validate(sup, value).is_err() {
                return Ok(false);
            }
        }
        return Ok(true);
    }
    if sup == &Value::Bool(false) || finite(sup).is_some() {
        return Ok(false);
    }
    if let Some(branches) = sub.get("anyOf").and_then(Value::as_array) {
        for branch in branches {
            if !fits(branch, sup, depth + 1)? {
                return Ok(false);
            }
        }
        return Ok(true);
    }
    if let Some(branches) = sup.get("anyOf").and_then(Value::as_array) {
        let mut base = sup.clone();
        base.as_object_mut()
            .expect("normalized union is an object")
            .remove("anyOf");
        if !fits(sub, &base, depth + 1)? {
            return Ok(false);
        }
        for branch in branches {
            if fits(sub, branch, depth + 1)? {
                return Ok(true);
            }
        }
        return Ok(false);
    }
    for key in CONSTRAINTS {
        if sup.get(key).is_some() && sub.get(key) != sup.get(key) {
            return Ok(false);
        }
    }
    let Some(sub_type) = sub.get("type").and_then(Value::as_str) else {
        return Ok(false);
    };
    let Some(sup_type) = sup.get("type").and_then(Value::as_str) else {
        return Ok(false);
    };
    if sub_type != sup_type && !(sub_type == "integer" && sup_type == "number") {
        return Ok(false);
    }
    if sub_type == "object" {
        let empty = Map::new();
        let sub_properties = sub
            .get("properties")
            .and_then(Value::as_object)
            .unwrap_or(&empty);
        let sup_properties = sup
            .get("properties")
            .and_then(Value::as_object)
            .unwrap_or(&empty);
        let sub_required = required(sub);
        if required(sup)
            .iter()
            .any(|name| !sub_required.contains(name))
        {
            return Ok(false);
        }
        for (name, wanted) in sup_properties {
            if let Some(offered) = sub_properties.get(name) {
                if !fits(offered, wanted, depth + 1)? {
                    return Ok(false);
                }
            } else if sub.get("additionalProperties") != Some(&Value::Bool(false))
                && !fits(&json!({}), wanted, depth + 1)?
            {
                return Ok(false);
            }
        }
        if sup.get("additionalProperties") == Some(&Value::Bool(false))
            && (sub.get("additionalProperties") != Some(&Value::Bool(false))
                || sub_properties
                    .keys()
                    .any(|name| !sup_properties.contains_key(name)))
        {
            return Ok(false);
        }
    }
    if sub_type == "array"
        && !fits(
            sub.get("items").unwrap_or(&json!({})),
            sup.get("items").unwrap_or(&json!({})),
            depth + 1,
        )?
    {
        return Ok(false);
    }
    Ok(true)
}

fn required(schema: &Value) -> Vec<&str> {
    schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}

/// Overapproximate values after both projections. Lost lower property bounds are dropped.
fn projected(consumer: &Value, provider: &Value) -> Result<Value> {
    if let Some(candidates) = finite(consumer) {
        let mut values = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for candidate in candidates {
            if validate(consumer, candidate).is_ok() {
                let sent = clean_arguments(consumer, candidate)?;
                let received = clean_arguments(provider, &sent)?;
                if seen.insert(canonical_json(&received)?) {
                    values.push(received);
                }
            }
        }
        return Ok(if values.is_empty() {
            Value::Bool(false)
        } else {
            json!({"enum":values})
        });
    }
    if consumer.get("anyOf").is_some() || provider.get("anyOf").is_some() {
        if validity(consumer) == validity(provider) {
            return Ok(consumer.clone());
        }
        return Err(mismatch(
            "union input projection requires an explicit proof or adapter",
        ));
    }
    let mut output = consumer.clone();
    if consumer.get("type").and_then(Value::as_str) == Some("object") {
        let empty = Map::new();
        let properties = consumer
            .get("properties")
            .and_then(Value::as_object)
            .unwrap_or(&empty);
        let provider_properties = provider
            .get("properties")
            .and_then(Value::as_object)
            .unwrap_or(&empty);
        let provider_object = provider.get("type").and_then(Value::as_str) == Some("object")
            || provider.get("properties").is_some()
            || provider.get("additionalProperties").is_some();
        let provider_strips =
            provider_object && provider.get("additionalProperties") != Some(&Value::Bool(true));
        let consumer_open = consumer.get("additionalProperties") == Some(&Value::Bool(true));
        let mut projected_properties = Map::new();
        for (name, schema) in properties {
            if provider_strips && !provider_properties.contains_key(name) {
                continue;
            }
            let target = provider_properties.get(name).unwrap_or(&Value::Null);
            projected_properties.insert(name.clone(), projected(schema, target)?);
        }
        if consumer_open && provider_strips {
            for name in provider_properties.keys() {
                projected_properties
                    .entry(name.clone())
                    .or_insert_with(|| json!({}));
            }
        }
        let removed = properties
            .keys()
            .any(|name| !projected_properties.contains_key(name));
        let required = required(consumer)
            .into_iter()
            .filter(|name| projected_properties.contains_key(*name))
            .map(Value::from)
            .collect();
        let object = output
            .as_object_mut()
            .expect("typed object schema is an object");
        object.insert("properties".into(), Value::Object(projected_properties));
        object.insert("required".into(), Value::Array(required));
        object.insert(
            "additionalProperties".into(),
            Value::Bool(consumer_open && !provider_strips),
        );
        if removed || (consumer_open && provider_strips) {
            object.remove("minProperties");
        }
    } else if consumer.get("type").and_then(Value::as_str) == Some("array") {
        let source_items = consumer.get("items").cloned().unwrap_or_else(|| json!({}));
        let target_items = provider.get("items").cloned().unwrap_or_else(|| json!({}));
        let items = projected(&source_items, &target_items)?;
        if crate::schema::normalization_signature(&source_items)?
            != crate::schema::normalization_signature(&target_items)?
        {
            // Projecting two distinct objects can collapse them to equal array items.
            output
                .as_object_mut()
                .expect("array schema is an object")
                .remove("uniqueItems");
        }
        output["items"] = items;
    }
    Ok(output)
}
