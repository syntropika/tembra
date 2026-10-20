use js_sys::{JsString, Reflect};
use serde_json::Value;
use tembra_core::RuntimeError;
use wasm_bindgen::{JsCast, JsValue, prelude::wasm_bindgen};

pub(crate) fn failure(error: RuntimeError) -> JsValue {
    let result = js_sys::Error::new(&error.message);
    // All fields originate from the typed Rust failure, rather than arbitrary JS objects.
    for (name, value) in [
        ("code", JsValue::from_str(error.code.as_str())),
        ("reason", JsValue::from_str(&error.reason)),
        (
            "detailsJson",
            JsValue::from_str(&error.details().to_string()),
        ),
    ] {
        let _ = Reflect::set(&result, &JsValue::from_str(name), &value);
    }
    result.into()
}

pub(crate) fn contract_failure(error: tembra_contracts::ContractError) -> JsValue {
    let path = error.path;
    let failure = failure(RuntimeError::invalid(error.reason, error.message));
    let _ = Reflect::set(
        &failure,
        &JsValue::from_str("path"),
        &JsValue::from_str(&path),
    );
    failure
}

pub(crate) fn text(value: &JsValue) -> Result<String, JsValue> {
    if !value.is_string() {
        return Err(failure(RuntimeError::invalid(
            "json_type",
            "expected a primitive JavaScript string",
        )));
    }
    let string: JsString = value.clone().unchecked_into();
    if string.length() > 1_048_576 {
        return Err(failure(RuntimeError::invalid(
            "json_size",
            "text exceeds the portable ingestion limit",
        )));
    }
    let units = (0..string.length()).map(|index| string.char_code_at(index) as u16);
    if char::decode_utf16(units).any(|unit| unit.is_err()) {
        return Err(failure(RuntimeError::invalid(
            "json_unicode",
            "unpaired UTF-16 surrogate",
        )));
    }
    string.as_string().ok_or_else(|| {
        failure(RuntimeError::invalid(
            "json_type",
            "string conversion failed",
        ))
    })
}

pub(crate) fn json(value: &JsValue) -> Result<Value, JsValue> {
    tembra_contracts::parse_json(&text(value)?).map_err(contract_failure)
}

/// Strictly parses raw JSON and returns its canonical portable representation.
///
/// # Errors
/// Rejects non-string inputs, unpaired UTF-16, invalid or nonportable JSON and parser limits.
#[wasm_bindgen(js_name = canonicalJson)]
pub fn canonical_json(raw: JsValue) -> Result<String, JsValue> {
    tembra_contracts::canonical_json(&json(&raw)?).map_err(contract_failure)
}

/// Validates a portable value with the shared Rust schema implementation.
///
/// # Errors
/// Rejects invalid text, unsupported schemas and values outside the schema.
#[wasm_bindgen(js_name = validateValue)]
pub fn validate_value(schema: JsValue, value: JsValue) -> Result<(), JsValue> {
    tembra_contracts::validate(&json(&schema)?, &json(&value)?).map_err(contract_failure)
}

/// Normalizes acquisition input using the shared schema cleaning/default policy.
///
/// # Errors
/// Rejects invalid text, unsupported cleaning policies and invalid normalized input.
#[wasm_bindgen(js_name = normalizeInput)]
pub fn normalize_input(schema: JsValue, value: JsValue) -> Result<String, JsValue> {
    let value = tembra_contracts::normalize_input(&json(&schema)?, &json(&value)?)
        .map_err(contract_failure)?;
    tembra_contracts::canonical_json(&value).map_err(contract_failure)
}

/// Verifies a complete peer descriptor and returns its checked fingerprint.
///
/// # Errors
/// Rejects malformed or unsupported descriptors and missing/mismatching fingerprints.
#[wasm_bindgen(js_name = verifyDescriptor)]
pub fn verify_descriptor(raw: JsValue) -> Result<String, JsValue> {
    let contract = tembra_contracts::Contract::from_json(&text(&raw)?).map_err(contract_failure)?;
    Ok(contract.fingerprint().to_owned())
}

/// Compiles a source schema into the same portable schema used by native contracts.
///
/// # Errors
/// Refuses invalid text, unsupported references/keywords and schema compilation limits.
#[wasm_bindgen(js_name = normalizeSchema)]
pub fn normalize_schema(raw: JsValue) -> Result<String, JsValue> {
    let value = tembra_contracts::normalize_schema(&json(&raw)?).map_err(contract_failure)?;
    tembra_contracts::canonical_json(&value).map_err(contract_failure)
}

/// Computes normalized acquisition identity through the shared Contract implementation.
///
/// # Errors
/// Refuses invalid text, unsupported acquisition schemas and invalid normalized input.
#[wasm_bindgen(js_name = canonicalIdentity)]
pub fn canonical_identity(schema: JsValue, input: JsValue) -> Result<String, JsValue> {
    let contract = tembra_contracts::Contract::local(
        "wasm.identity",
        1,
        json(&schema)?,
        std::collections::BTreeMap::new(),
    )
    .map_err(contract_failure)?;
    contract.identity(&json(&input)?).map_err(contract_failure)
}

/// Returns the verified descriptor's canonical fingerprint input bytes.
///
/// # Errors
/// Refuses invalid descriptors exactly as verifyDescriptor; the fingerprint is omitted only
/// from the canonical hash input, never from peer verification.
#[wasm_bindgen(js_name = descriptorCanonical)]
pub fn descriptor_canonical(raw: JsValue) -> Result<String, JsValue> {
    let contract = tembra_contracts::Contract::from_json(&text(&raw)?).map_err(contract_failure)?;
    let mut value = serde_json::to_value(contract).map_err(|_| {
        failure(RuntimeError::invalid(
            "descriptor",
            "cannot encode verified descriptor",
        ))
    })?;
    if let Some(fields) = value.as_object_mut() {
        fields.remove("fingerprint");
    }
    tembra_contracts::canonical_json(&value).map_err(contract_failure)
}
