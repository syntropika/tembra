//! Metadata-only adapter protocol. User arguments, state values and callbacks never enter JSON here.
pub mod protocol;
use protocol::{ActionType, EffectType, KernelOp, StepType};

use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use tembra_core::*;

pub use tembra_core as core;

#[derive(Clone, Default)]
pub struct Kernel {
    pub host: Host,
}

impl Kernel {
    /// Create a metadata bridge with a fresh kernel host.
    pub fn new() -> Self {
        Self::default()
    }

    /// Recheck context authority and admit a call through the existing command validators.
    pub fn admit_call_batch(&self, request: &str) -> Result<Value> {
        let value = parse_request(request)?;
        self.execute(&json!({"op":"check-context","root":value["root"],"call":value["parent"]}))?;
        self.execute(&json!({"op":"begin-call","reference":value["reference"],"root":value["root"],"context":value["context"],"method":value["method"]}))
    }

    /// Finish a physical call and return its actions and deadline in one adapter exchange.
    pub fn finish_call_batch(&self, call: Id, now: u64) -> Result<Value> {
        self.host.tick(now)?;
        self.host.finish_call(call)?;
        Ok(
            json!({"actions":self.host.drain().into_iter().map(encode::action_value).collect::<Vec<_>>(),"deadline":self.host.next_deadline()}),
        )
    }

    /// Converts a metadata request to a result envelope. Malformed input executes no command.
    pub fn request(&self, request: &str) -> String {
        let result = parse_request(request).and_then(|value| self.execute(&value));
        match result {
            Ok(value) => json!({ "ok": value }),
            Err(error) => json!({ "error": encode::error_value(error) }),
        }
        .to_string()
    }
}

mod commands;
mod decode;
mod encode;

/// Apply the same metadata size and syntax limits to individual and batched exchanges.
fn parse_request(request: &str) -> Result<Value> {
    if request.len() > 1024 * 1024 {
        return Err(Error::invalid(
            Reason::MetadataSize,
            "metadata exceeds one mebibyte",
        ));
    }
    serde_json::from_str(request)
        .map_err(|error| Error::invalid(Reason::Metadata, error.to_string()))
}
