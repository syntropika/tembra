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

    /// Converts a metadata request to a result envelope. Malformed input executes no command.
    pub fn request(&self, request: &str) -> String {
        let result = if request.len() > 1024 * 1024 {
            Err(Error::invalid(
                Reason::MetadataSize,
                "metadata exceeds one mebibyte",
            ))
        } else {
            serde_json::from_str::<Value>(request)
                .map_err(|e| Error::invalid(Reason::Metadata, e.to_string()))
                .and_then(|value| self.execute(&value))
        };
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
