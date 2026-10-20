use js_sys::{Function, Reflect};
use serde_json::Value;
use tembra_core::{ErrorCode, RuntimeError};
use wasm_bindgen::{JsCast, JsValue};

use crate::boundary::{json, text};

pub(crate) use crate::intrinsics::{object_keys_owned, observe_owned, reject_async};

pub(crate) fn error(reason: &str, message: &str) -> RuntimeError {
    RuntimeError::invalid(reason, message)
}
pub(crate) fn js_error(value: JsValue) -> RuntimeError {
    let field = |name: &str| Reflect::get(&value, &JsValue::from_str(name)).ok();
    let code = field("code")
        .and_then(|code| code.as_string())
        .and_then(|code| serde_json::from_value::<ErrorCode>(Value::String(code)).ok());
    let message = field("message").and_then(|message| text(&message).ok());
    if let (Some(code), Some(reason), Some(message)) = (
        code,
        field("reason").and_then(|reason| text(&reason).ok()),
        message.clone(),
    ) {
        let details = field("detailsJson").and_then(|details| json(&details).ok());
        return RuntimeError::new(code, reason, message)
            .with_details(details.unwrap_or(Value::Null));
    }
    RuntimeError::invalid(
        "javascript",
        message.unwrap_or_else(|| "JavaScript callback failed".into()),
    )
}
pub(crate) fn strict_json(value: &JsValue) -> Result<Value, RuntimeError> {
    json(value).map_err(js_error)
}
pub(crate) fn string(value: &JsValue) -> Result<String, RuntimeError> {
    text(value).map_err(js_error)
}
pub(crate) fn field(value: &JsValue, key: &str) -> Result<JsValue, RuntimeError> {
    Reflect::get(value, &JsValue::from_str(key)).map_err(js_error)
}
pub(crate) fn function(value: JsValue) -> Result<Function, RuntimeError> {
    value
        .dyn_into()
        .map_err(|_| error("callback", "expected a callable"))
}
pub(crate) fn record_keys(value: &JsValue) -> Result<js_sys::Array, RuntimeError> {
    if !value.is_object() || value.is_null() || js_sys::Array::is_array(value) {
        return Err(error("implementation", "methods must be an object"));
    }
    object_keys_owned(value).map_err(js_error)
}
pub(crate) fn synchronous(value: JsValue) -> Result<JsValue, RuntimeError> {
    reject_async(&value)
        .map_err(|_| error("async_declaration", "callback must return synchronously"))
}
pub(crate) fn version(value: f64) -> Result<u64, RuntimeError> {
    if value.is_finite() && value.fract() == 0.0 && (1.0..=9_007_199_254_740_991.0).contains(&value)
    {
        Ok(value as u64)
    } else {
        Err(error("version", "version must be a positive safe integer"))
    }
}
pub(crate) fn contract(
    raw: &JsValue,
) -> Result<std::rc::Rc<tembra_core::contracts::Contract>, RuntimeError> {
    tembra_core::contracts::Contract::from_json(&string(raw)?)
        .map(std::rc::Rc::new)
        .map_err(|cause| error(&cause.reason, &cause.message))
}
pub(crate) fn payload(value: Value) -> Result<Option<Value>, RuntimeError> {
    let fields = value
        .as_object()
        .ok_or_else(|| error("payload", "expected presence envelope"))?;
    match fields.get("present") {
        Some(Value::Bool(false)) if fields.len() == 1 => Ok(None),
        Some(Value::Bool(true)) if fields.len() == 2 && fields.contains_key("value") => {
            Ok(Some(fields["value"].clone()))
        }
        _ => Err(error("payload", "invalid presence envelope")),
    }
}
pub(crate) fn envelope(value: Option<Value>) -> Value {
    match value {
        Some(value) => serde_json::json!({"present":true,"value":value}),
        None => serde_json::json!({"present":false}),
    }
}
