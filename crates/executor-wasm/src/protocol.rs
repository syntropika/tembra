//! Private host-operation protocol shared by the native executor and the guest interpreter.
//!
//! The guest crate includes this exact file through `#[path]`, so both sides agree on one
//! typed control vocabulary. Only control metadata is typed here; WIT-validated payloads
//! (`initial`, `value`, `args`) stay JSON values. A `scope` field names an environment handle
//! table, never a host context: the host accepts only its own table or its issued parent's.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::borrow::Cow;

/// Interpreter interrupt polls granted to one callback and, separately, to its cleanup.
pub const DEFAULT_INSTRUCTION_TICKS: u32 = 10_000;

/// Source revision and VM bounds delivered by `Operation::Load`.
///
/// Hosts may omit `instruction_ticks`; the guest then applies `DEFAULT_INSTRUCTION_TICKS`.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Load<'a> {
    #[serde(borrow)]
    pub source: Cow<'a, str>,
    pub heap_bytes: usize,
    pub contract: Value,
    #[serde(default = "default_instruction_ticks")]
    pub instruction_ticks: u32,
}

/// Serde default for hosts that do not configure an instruction bound.
fn default_instruction_ticks() -> u32 {
    DEFAULT_INSTRUCTION_TICKS
}

/// Host-selected callbacks contain typed control fields and WIT-checked argument values.
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum GuestCommand<'a> {
    Create {
        environment: u64,
        service: Option<&'a str>,
    },
    Invoke {
        environment: u64,
        service: Option<&'a str>,
        method: &'a str,
        args: &'a Value,
    },
    Validate {
        callback: u32,
        value: &'a Value,
    },
    Callback {
        callback: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        value: Option<u32>,
    },
    Release {
        environment: u64,
    },
}

/// Environment handle table selected by a guest request.
pub type ScopeId = u64;

/// One handle issued by a host table, optionally qualified by the table that issued it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Handle {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<ScopeId>,
    pub id: u32,
}

/// Console severities accepted from guest code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Log,
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    /// Return the wire name used by log observers.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Log => "log",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

/// Synchronous host operations. Unknown operation names decode to `Unsupported`;
/// a known operation with missing, mistyped or extra fields is a decode error.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "operation", deny_unknown_fields)]
pub enum Request {
    /// Attributed diagnostic record; requires no callback authority.
    #[serde(rename = "console")]
    Console { level: LogLevel, message: String },
    #[serde(rename = "address")]
    Address {},
    #[serde(rename = "orb")]
    Orb {},
    #[serde(rename = "signal.cancelled")]
    SignalCancelled {},
    #[serde(rename = "context.check")]
    ContextCheck {},
    #[serde(rename = "context.principal")]
    ContextPrincipal {},
    #[serde(rename = "context.cancel")]
    ContextCancel {},
    /// Declarations always write into the declaring environment's own table.
    #[serde(rename = "state.declare")]
    StateDeclare {
        #[serde(default)]
        initial: Value,
        version: u32,
        #[serde(default)]
        validate: Option<u32>,
    },
    /// `known` names the kernel revision whose protected value the guest already retains.
    #[serde(rename = "state.read")]
    StateRead {
        #[serde(default)]
        scope: Option<ScopeId>,
        id: u32,
        #[serde(default)]
        known: Option<String>,
    },
    #[serde(rename = "state.update")]
    StateUpdate {
        #[serde(default)]
        scope: Option<ScopeId>,
        id: u32,
        #[serde(default)]
        value: Value,
    },
    #[serde(rename = "reference.declare")]
    ReferenceDeclare { service: String, key: Vec<String> },
    #[serde(rename = "reference.release")]
    ReferenceRelease {
        #[serde(default)]
        scope: Option<ScopeId>,
        id: u32,
    },
    #[serde(rename = "reference.status")]
    ReferenceStatus {
        #[serde(default)]
        scope: Option<ScopeId>,
        id: u32,
    },
    /// `known` names the kernel revision whose protected value the guest already retains.
    #[serde(rename = "reference.state")]
    ReferenceState {
        #[serde(default)]
        scope: Option<ScopeId>,
        id: u32,
        #[serde(default)]
        known: Option<String>,
    },
    #[serde(rename = "resource.declare")]
    ResourceDeclare {
        name: String,
        version: u32,
        open: u32,
        close: u32,
        #[serde(default)]
        dependencies: Vec<Handle>,
    },
    #[serde(rename = "resource.read")]
    ResourceRead {
        #[serde(default)]
        scope: Option<ScopeId>,
        id: u32,
    },
    /// Any operation name outside this vocabulary.
    #[serde(other, skip_serializing)]
    Unsupported,
}

/// Asynchronous host operations, awaited outside the guest heap.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "operation", deny_unknown_fields)]
pub enum AsyncRequest {
    #[serde(rename = "reference.ready")]
    ReferenceReady {
        #[serde(default)]
        scope: Option<ScopeId>,
        id: u32,
    },
    #[serde(rename = "reference.call")]
    ReferenceCall {
        #[serde(default)]
        scope: Option<ScopeId>,
        id: u32,
        method: String,
        args: Value,
    },
    #[serde(rename = "filesystem.read-text")]
    ReadText { path: String },
    #[serde(rename = "filesystem.write-text")]
    WriteText { path: String, value: String },
    #[serde(rename = "transport.exchange")]
    Exchange {
        ip: String,
        port: u16,
        request: String,
    },
    #[serde(rename = "wait")]
    Wait { milliseconds: u32 },
    /// Any operation name outside this vocabulary.
    #[serde(other, skip_serializing)]
    Unsupported,
}

/// Snapshot returned for state and reference-state reads. A read naming the current revision as
/// `known` receives only `revision` and `version`; the unchanged value is not resent.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Snapshot {
    pub value: Value,
    pub revision: String,
    pub version: u32,
}

/// Address returned to declaration factories.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Address {
    pub key: Vec<String>,
    pub principal: String,
}

/// Longest cooperative timer a callback may request.
pub const WAIT_LIMIT_MILLISECONDS: u32 = 5000;

impl Request {
    /// Decode one synchronous packet, labelling structural failures as malformed.
    pub fn decode(packet: &str) -> Result<Self, String> {
        serde_json::from_str(packet).map_err(|error| format!("malformed host request: {error}"))
    }

    /// Encode a guest-issued request.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn encode(&self) -> String {
        serde_json::to_string(self).expect("host requests always serialize")
    }

    /// Diagnostics and reads of already issued values stay usable after cancellation.
    pub fn survives_cancellation(&self) -> bool {
        matches!(
            self,
            Self::Console { .. }
                | Self::SignalCancelled {}
                | Self::StateRead { .. }
                | Self::ResourceRead { .. }
                | Self::ReferenceState { .. }
                | Self::ReferenceStatus { .. }
                | Self::ReferenceRelease { .. }
        )
    }
}

impl AsyncRequest {
    /// Physical I/O remains owned even after the guest stops awaiting its reply.
    pub fn is_io(&self) -> bool {
        matches!(
            self,
            Self::ReadText { .. } | Self::WriteText { .. } | Self::Exchange { .. }
        )
    }

    /// Decode one asynchronous packet, labelling structural failures as malformed.
    pub fn decode(packet: &str) -> Result<Self, String> {
        serde_json::from_str(packet)
            .map_err(|error| format!("malformed asynchronous host request: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decodes_scoped_handles_and_payloads() {
        let request =
            Request::decode(r#"{"operation":"state.update","scope":7,"id":2,"value":{"a":[1]}}"#)
                .unwrap();
        assert_eq!(
            request,
            Request::StateUpdate {
                scope: Some(7),
                id: 2,
                value: json!({"a":[1]}),
            }
        );
        assert_eq!(
            Request::decode(r#"{"operation":"state.read","id":1}"#).unwrap(),
            Request::StateRead {
                scope: None,
                id: 1,
                known: None,
            }
        );
        assert_eq!(
            Request::decode(r#"{"operation":"reference.state","id":1,"known":"7"}"#).unwrap(),
            Request::ReferenceState {
                scope: None,
                id: 1,
                known: Some("7".into()),
            }
        );
        assert!(Request::decode(r#"{"operation":"state.read","id":1,"known":7}"#).is_err());
        assert!(Request::decode(r#"{"operation":"state.update","id":1,"known":"7"}"#).is_err());
    }

    #[test]
    fn optional_validator_and_payload_default() {
        assert_eq!(
            Request::decode(r#"{"operation":"state.declare","version":1}"#).unwrap(),
            Request::StateDeclare {
                initial: Value::Null,
                version: 1,
                validate: None,
            }
        );
        assert_eq!(
            Request::decode(
                r#"{"operation":"state.declare","initial":3,"version":1,"validate":null}"#
            )
            .unwrap(),
            Request::StateDeclare {
                initial: json!(3),
                version: 1,
                validate: None,
            }
        );
    }

    #[test]
    fn unknown_operations_are_unsupported() {
        for packet in [
            r#"{"operation":"filesystem"}"#,
            r#"{"operation":"network"}"#,
            r#"{"operation":"reference.call","id":1,"method":"m","args":[]}"#,
        ] {
            assert_eq!(Request::decode(packet).unwrap(), Request::Unsupported);
        }
        assert_eq!(
            AsyncRequest::decode(r#"{"operation":"state.read","id":1}"#).unwrap(),
            AsyncRequest::Unsupported
        );
    }

    #[test]
    fn malformed_requests_are_rejected() {
        for packet in [
            "not json",
            r#"{"id":1}"#,
            r#"{"operation":"state.read"}"#,
            r#"{"operation":"state.read","id":-1}"#,
            r#"{"operation":"state.read","id":1.5}"#,
            r#"{"operation":"state.read","id":4294967296}"#,
            r#"{"operation":"state.read","id":"1"}"#,
            r#"{"operation":"state.read","id":1,"scope":"7"}"#,
            r#"{"operation":"state.read","id":1,"context":3}"#,
            r#"{"operation":"address","scope":1}"#,
            r#"{"operation":"state.declare","scope":1,"version":1}"#,
            r#"{"operation":"reference.declare","service":"s","key":"main"}"#,
            r#"{"operation":"resource.declare","name":"r","version":1,"open":1,"close":2,"dependencies":[{"id":1,"extra":0}]}"#,
            r#"{"operation":"console","level":"trace","message":"x"}"#,
        ] {
            let error = Request::decode(packet).unwrap_err();
            assert!(
                error.starts_with("malformed host request"),
                "{packet}: {error}"
            );
        }
        for packet in [
            r#"{"operation":"wait","milliseconds":-1}"#,
            r#"{"operation":"reference.call","id":1,"method":"m"}"#,
            r#"{"operation":"reference.ready","id":1,"extra":true}"#,
        ] {
            assert!(AsyncRequest::decode(packet).is_err(), "{packet}");
        }
    }

    #[test]
    fn guest_requests_round_trip() {
        let console = Request::Console {
            level: LogLevel::Warn,
            message: "hi".into(),
        };
        assert_eq!(
            console.encode(),
            r#"{"operation":"console","level":"warn","message":"hi"}"#
        );
        assert_eq!(Request::decode(&console.encode()).unwrap(), console);
    }

    #[test]
    fn load_packets_default_ticks_and_refuse_unknown_fields() {
        let packet = serde_json::to_string(&Load {
            source: "let a = \"\\n\";".into(),
            heap_bytes: 64,
            contract: json!({"services": {}}),
            instruction_ticks: DEFAULT_INSTRUCTION_TICKS,
        })
        .unwrap();
        assert_eq!(
            packet,
            r#"{"source":"let a = \"\\n\";","heap_bytes":64,"contract":{"services":{}},"instruction_ticks":10000}"#
        );
        let decoded: Load<'_> = serde_json::from_str(&packet).unwrap();
        assert_eq!(decoded.source, "let a = \"\\n\";");
        assert_eq!(decoded.instruction_ticks, DEFAULT_INSTRUCTION_TICKS);
        let omitted: Load<'_> =
            serde_json::from_str(r#"{"source":"","heap_bytes":1,"contract":null}"#).unwrap();
        assert_eq!(omitted.instruction_ticks, DEFAULT_INSTRUCTION_TICKS);
        let configured: Load<'_> = serde_json::from_str(
            r#"{"source":"","heap_bytes":1,"contract":null,"instruction_ticks":50}"#,
        )
        .unwrap();
        assert_eq!(configured.instruction_ticks, 50);
        assert!(
            serde_json::from_str::<Load<'_>>(
                r#"{"source":"","heap_bytes":1,"contract":null,"ticks":50}"#
            )
            .is_err()
        );
    }

    #[test]
    fn resource_dependencies_carry_their_issuing_scope() {
        assert_eq!(
            Request::decode(
                r#"{"operation":"resource.declare","name":"r","version":2,"open":1,"close":2,"dependencies":[{"scope":4,"id":9}]}"#
            )
            .unwrap(),
            Request::ResourceDeclare {
                name: "r".into(),
                version: 2,
                open: 1,
                close: 2,
                dependencies: vec![Handle {
                    scope: Some(4),
                    id: 9
                }],
            }
        );
    }
}
