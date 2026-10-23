//! Constructor-only browser projections of checked core policy.
use std::{collections::BTreeMap, time::Duration};
use tembra_core::{RuntimeError, rpc::ConsumerLingerOptions};
use wasm_bindgen::JsValue;

use crate::boundary::{failure, json};

pub(crate) fn consumer_linger(options: &JsValue) -> Result<ConsumerLingerOptions, JsValue> {
    let value = json(options)?;
    let object = value.as_object().ok_or_else(invalid)?;
    if object
        .keys()
        .any(|key| key != "consumerLinger" && key != "observability")
    {
        return Err(invalid());
    }
    let Some(policy) = object.get("consumerLinger") else {
        return Ok(ConsumerLingerOptions::default());
    };
    let policy = policy.as_object().ok_or_else(invalid)?;
    if policy.keys().any(|key| key != "types" && key != "capacity") {
        return Err(invalid());
    }
    let types = policy
        .get("types")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(invalid)?;
    let types = types
        .iter()
        .map(|(name, value)| {
            value
                .as_u64()
                .map(|milliseconds| (name.clone(), Duration::from_millis(milliseconds)))
                .ok_or_else(invalid)
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    let capacity = match policy.get("capacity") {
        None => ConsumerLingerOptions::default().capacity(),
        Some(value) => value
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(invalid)?,
    };
    ConsumerLingerOptions::new(types, capacity).map_err(failure)
}

fn invalid() -> JsValue {
    failure(RuntimeError::invalid(
        "consumer_linger",
        "expected checked consumerLinger types in whole milliseconds and optional capacity",
    ))
}
