use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub type Result<T> = std::result::Result<T, RuntimeError>;

/// Caller-visible categories shared by local calls and later transport adapters.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Aborted,
    ContractMismatch,
    Forbidden,
    Gone,
    Invalid,
    NotInVariant,
    NotReady,
    Released,
    Superseded,
    Unavailable,
    UpgradeConflict,
    UpgradeRequired,
}

impl ErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Aborted => "aborted",
            Self::ContractMismatch => "contract_mismatch",
            Self::Forbidden => "forbidden",
            Self::Gone => "gone",
            Self::Invalid => "invalid",
            Self::NotInVariant => "not_in_variant",
            Self::NotReady => "not_ready",
            Self::Released => "released",
            Self::Superseded => "superseded",
            Self::Unavailable => "unavailable",
            Self::UpgradeConflict => "upgrade_conflict",
            Self::UpgradeRequired => "upgrade_required",
        }
    }

    pub const fn retryable(self) -> bool {
        matches!(self, Self::Superseded | Self::UpgradeConflict)
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A structured failure with the same required fields in local and wire JSON.
///
/// Details remain owned immutable data to readers. Replacing them does not change the
/// category, reason or message; cloning preserves nested causes independently.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RuntimeError {
    pub code: ErrorCode,
    pub reason: String,
    pub message: String,
    #[serde(with = "details")]
    details: Option<Box<Value>>,
}

impl RuntimeError {
    pub fn new(code: ErrorCode, reason: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code,
            reason: reason.into(),
            message: message.into(),
            details: None,
        }
    }

    /// Reads the structured details, including explicit null when no details were supplied.
    pub fn details(&self) -> &Value {
        self.details.as_deref().unwrap_or(&Value::Null)
    }

    /// Replaces only the details and returns this error for construction at a boundary.
    pub fn with_details(mut self, details: Value) -> Self {
        self.set_details(details);
        self
    }

    /// Replaces only the owned details; previously cloned errors remain unchanged.
    pub fn set_details(&mut self, details: Value) {
        if details.is_null() {
            self.details = None;
        } else if let Some(current) = &mut self.details {
            **current = details;
        } else {
            self.details = Some(Box::new(details));
        }
    }

    pub(crate) fn into_parts(self) -> (ErrorCode, String, String, Value) {
        (
            self.code,
            self.reason,
            self.message,
            self.details.map_or(Value::Null, |details| *details),
        )
    }

    pub fn invalid(reason: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Invalid, reason, message)
    }

    pub fn unavailable(reason: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Unavailable, reason, message)
    }

    pub fn aborted(reason: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Aborted, reason, message)
    }

    pub fn retryable(&self) -> bool {
        self.code.retryable()
    }
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.reason.is_empty() {
            write!(formatter, "{}: {}", self.code, self.message)
        } else {
            write!(formatter, "{}/{}: {}", self.code, self.reason, self.message)
        }
    }
}

impl std::error::Error for RuntimeError {}

impl From<String> for RuntimeError {
    fn from(reason: String) -> Self {
        Self::aborted(reason, "operation cancelled")
    }
}

impl From<&str> for RuntimeError {
    fn from(reason: &str) -> Self {
        Self::from(reason.to_owned())
    }
}

// Custom field decoding keeps details required, even though null uses an allocation-free None.
mod details {
    use super::*;

    pub(super) fn serialize<S: serde::Serializer>(
        details: &Option<Box<Value>>,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        details
            .as_deref()
            .unwrap_or(&Value::Null)
            .serialize(serializer)
    }

    pub(super) fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Option<Box<Value>>, D::Error> {
        let details = Value::deserialize(deserializer)?;
        Ok((!details.is_null()).then(|| Box::new(details)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::protocol::WireError;
    use serde_json::json;

    #[test]
    fn local_and_wire_error_json_keep_required_structured_details() {
        for details in [
            Value::Null,
            json!({"cause": {"attempt": 2}, "path": ["a", 0]}),
        ] {
            let error = RuntimeError::new(ErrorCode::Forbidden, "policy", "refused")
                .with_details(details.clone());
            let expected = json!({
                "code": "forbidden", "reason": "policy", "message": "refused",
                "details": details,
            });
            assert_eq!(serde_json::to_value(&error).unwrap(), expected);
            assert_eq!(
                serde_json::from_value::<RuntimeError>(expected.clone()).unwrap(),
                error
            );
            let wire = WireError::from(error.clone());
            assert_eq!(serde_json::to_value(&wire).unwrap(), expected);
            assert_eq!(RuntimeError::from(wire), error);
        }
        let missing = json!({"code": "forbidden", "reason": "policy", "message": "refused"});
        assert!(serde_json::from_value::<RuntimeError>(missing.clone()).is_err());
        assert!(serde_json::from_value::<WireError>(missing).is_err());
    }

    #[test]
    fn replacing_details_preserves_other_fields_and_independent_clones() {
        let mut error = RuntimeError::aborted("stopped", "original")
            .with_details(json!({"nested": {"reason": "first"}}));
        let saved = error.clone();
        error.set_details(json!({"nested": {"reason": "second"}}));
        assert_eq!(saved.details()["nested"]["reason"], "first");
        assert_eq!(error.details()["nested"]["reason"], "second");
        assert_eq!(error.code, saved.code);
        assert_eq!(error.reason, saved.reason);
        assert_eq!(error.message, saved.message);
        assert_eq!(RuntimeError::invalid("x", "y").details(), &Value::Null);
    }
}
