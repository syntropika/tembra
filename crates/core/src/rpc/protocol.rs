use std::{collections::BTreeSet, rc::Rc};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tembra_contracts::Contract;

use crate::{ErrorCode, Result, RuntimeError, delta::DeltaBatch};

pub const MAX_SAFE: u64 = 9_007_199_254_740_991;
pub const MAX_FRAME: usize = 1_048_576;

macro_rules! checked_id {
    ($name:ident, $minimum:expr) => {
        #[derive(
            Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(try_from = "u64", into = "u64")]
        pub struct $name(u64);
        impl $name {
            /// Checks the portable integer domain for this identifier.
            ///
            /// # Errors
            /// Refuses zero for positive identifiers and numbers above the safe integer limit.
            pub fn new(value: u64) -> Result<Self> {
                if ($minimum..=MAX_SAFE).contains(&value) {
                    Ok(Self(value))
                } else {
                    Err(RuntimeError::invalid(
                        "rpc_id",
                        "identifier is outside its safe integer domain",
                    ))
                }
            }
            pub const fn get(self) -> u64 {
                self.0
            }
        }
        impl TryFrom<u64> for $name {
            type Error = RuntimeError;
            fn try_from(value: u64) -> Result<Self> {
                Self::new(value)
            }
        }
        impl From<$name> for u64 {
            fn from(value: $name) -> Self {
                value.0
            }
        }
    };
}
checked_id!(ConnectionId, 1);
checked_id!(PositiveId, 1);
checked_id!(HoldId, 1);
checked_id!(CallId, 1);
checked_id!(UnitId, 1);
checked_id!(StreamId, 1);
checked_id!(AttachmentId, 1);
checked_id!(SafeU64, 0);

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "OriginDto", into = "OriginDto")]
pub struct RootOrigin {
    pub(crate) host: String,
    pub(crate) incarnation: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OriginDto {
    host: String,
    incarnation: String,
}

impl RootOrigin {
    /// Creates an embedding-issued identity, never an authentication decision.
    ///
    /// # Errors
    /// Refuses an empty/oversized name or an incarnation other than 32 lowercase hex digits.
    pub fn new(host: impl Into<String>, incarnation: impl Into<String>) -> Result<Self> {
        let host = host.into();
        let incarnation = incarnation.into();
        if host.is_empty()
            || host.len() > 256
            || incarnation.len() != 32
            || !incarnation
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(RuntimeError::invalid(
                "rpc_identity",
                "invalid host name or incarnation",
            ));
        }
        Ok(Self { host, incarnation })
    }
    pub fn host(&self) -> &str {
        &self.host
    }
    pub fn incarnation(&self) -> &str {
        &self.incarnation
    }
}
impl TryFrom<OriginDto> for RootOrigin {
    type Error = RuntimeError;
    fn try_from(value: OriginDto) -> Result<Self> {
        Self::new(value.host, value.incarnation)
    }
}
impl From<RootOrigin> for OriginDto {
    fn from(value: RootOrigin) -> Self {
        Self {
            host: value.host,
            incarnation: value.incarnation,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RootId {
    pub origin: RootOrigin,
    pub sequence: PositiveId,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RootRef {
    pub identity: RootId,
    pub code_version: PositiveId,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(
    tag = "presence",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum Payload {
    Absent,
    Present(Value),
}
impl<'de> Deserialize<'de> for Payload {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let object = value
            .as_object()
            .ok_or_else(|| serde::de::Error::custom("payload must be an object"))?;
        match object.get("presence").and_then(Value::as_str) {
            Some("absent") if object.len() == 1 => Ok(Self::Absent),
            Some("present") if object.len() == 2 && object.contains_key("value") => {
                Ok(Self::Present(object["value"].clone()))
            }
            _ => Err(serde::de::Error::custom(
                "payload presence has missing or forbidden fields",
            )),
        }
    }
}
impl From<Option<Value>> for Payload {
    fn from(value: Option<Value>) -> Self {
        value.map_or(Self::Absent, Self::Present)
    }
}
impl From<Payload> for Option<Value> {
    fn from(value: Payload) -> Self {
        match value {
            Payload::Absent => None,
            Payload::Present(value) => Some(value),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WireError {
    pub code: ErrorCode,
    pub reason: String,
    pub message: String,
    pub details: Value,
}
impl From<RuntimeError> for WireError {
    fn from(value: RuntimeError) -> Self {
        let (code, reason, message, details) = value.into_parts();
        Self {
            code,
            reason,
            message,
            details,
        }
    }
}
impl From<WireError> for RuntimeError {
    fn from(value: WireError) -> Self {
        Self::new(value.code, value.reason, value.message).with_details(value.details)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "status",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum CallOutcome {
    Success(Payload),
    Error(WireError),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum PeerHello {
    Client {},
    Host {
        identity: RootOrigin,
        code_version: PositiveId,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReceiveOffer {
    pub max_message_bytes: SafeU64,
    pub max_unit_bytes: SafeU64,
    pub state_window_bytes: SafeU64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Hello {
    pub protocol: String,
    pub version: SafeU64,
    pub descriptor_version: SafeU64,
    pub delta_codec: SafeU64,
    pub peer: PeerHello,
    pub receive: ReceiveOffer,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ForwardedCaller {
    pub principal: Value,
    pub origin: String,
    pub authorization_generation: SafeU64,
}
#[derive(Clone)]
pub(crate) struct BindRequest {
    pub target: String,
    pub consumer: Rc<Contract>,
    pub input: Value,
    pub state: bool,
    pub caller: Option<ForwardedCaller>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HoldFrame {
    pub hold: HoldId,
    pub target: String,
    pub consumer: Value,
    pub input: Value,
    pub state: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ForwardHoldFrame {
    pub hold: HoldId,
    pub target: String,
    pub consumer: Value,
    pub input: Value,
    pub state: bool,
    pub caller: ForwardedCaller,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum BindingMode {
    Direct,
    Adapted { consumer_fingerprint: String },
}
impl<'de> Deserialize<'de> for BindingMode {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let object = value
            .as_object()
            .ok_or_else(|| serde::de::Error::custom("binding mode must be an object"))?;
        match object.get("mode").and_then(Value::as_str) {
            Some("direct") if object.len() == 1 => Ok(Self::Direct),
            Some("adapted") if object.len() == 2 => {
                let fingerprint = object
                    .get("consumer_fingerprint")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        serde::de::Error::custom("adapter consumer fingerprint must be a string")
                    })?;
                Ok(Self::Adapted {
                    consumer_fingerprint: fingerprint.into(),
                })
            }
            _ => Err(serde::de::Error::custom(
                "binding mode has missing or forbidden fields",
            )),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StateKey {
    pub hold: HoldId,
    pub unit: UnitId,
    pub member: String,
    pub stream: StreamId,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StateMember {
    pub member: String,
    pub stream: StreamId,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ShapeFrame {
    pub hold: HoldId,
    pub unit: UnitId,
    pub consumer_fingerprint: String,
    pub provider: Value,
    pub mode: BindingMode,
    pub variant: String,
    pub members: Vec<String>,
    pub state: Vec<StateMember>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WireSnapshot {
    pub revision: SafeU64,
    pub value: Value,
}
#[derive(Clone, Debug)]
pub(crate) enum ReplicaFrame {
    Snapshot {
        key: StateKey,
        snapshot: WireSnapshot,
    },
    Delta {
        key: StateKey,
        batch: DeltaBatch,
    },
    Reset {
        key: StateKey,
        snapshot: WireSnapshot,
    },
    Error {
        key: StateKey,
        error: RuntimeError,
    },
}
impl ReplicaFrame {
    pub fn key(&self) -> &StateKey {
        match self {
            Self::Snapshot { key, .. }
            | Self::Delta { key, .. }
            | Self::Reset { key, .. }
            | Self::Error { key, .. } => key,
        }
    }
    pub fn revision(&self) -> Option<u64> {
        match self {
            Self::Snapshot { snapshot, .. } | Self::Reset { snapshot, .. } => {
                Some(snapshot.revision.get())
            }
            Self::Delta { batch, .. } => Some(batch.result_revision()),
            Self::Error { .. } => None,
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub(crate) enum ReplicaDisposition {
    Ignore,
    Accepted,
    Resync { seen_revision: Option<u64> },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Frame {
    ReloadPrepare {
        transaction: PositiveId,
        code_version: PositiveId,
        facets: Vec<super::reload::RemoteFacet>,
        remove: Vec<String>,
        timing: super::reload::Timing,
        catchup: bool,
    },
    ReloadVote {
        transaction: PositiveId,
        code_version: PositiveId,
        outcome: super::reload::Vote,
    },
    ReloadCommit {
        transaction: PositiveId,
        code_version: PositiveId,
    },
    ReloadAck {
        transaction: PositiveId,
        code_version: PositiveId,
    },
    ReloadAbort {
        transaction: PositiveId,
        error: WireError,
    },
    ReloadRetire {
        code_version: PositiveId,
    },
    Hello {
        hello: Hello,
    },
    Bye {
        error: WireError,
    },
    Ping {
        nonce: HoldId,
    },
    Pong {
        nonce: HoldId,
    },
    Hold(HoldFrame),
    ForwardHold(ForwardHoldFrame),
    Shape(ShapeFrame),
    Gone {
        hold: HoldId,
        error: WireError,
    },
    Release {
        hold: HoldId,
    },
    Subscribe {
        hold: HoldId,
    },
    Unsubscribe {
        hold: HoldId,
    },
    Call {
        call: CallId,
        hold: HoldId,
        member: String,
        args: Payload,
    },
    TrustedCall {
        call: CallId,
        hold: HoldId,
        member: String,
        args: Payload,
        root: RootRef,
    },
    Result {
        call: CallId,
        outcome: CallOutcome,
    },
    Cancel {
        call: CallId,
    },
    Snapshot {
        key: StateKey,
        snapshot: WireSnapshot,
    },
    Delta {
        key: StateKey,
        batch: DeltaBatch,
    },
    Reset {
        key: StateKey,
        snapshot: WireSnapshot,
    },
    StateError {
        key: StateKey,
        error: WireError,
    },
    Resync {
        key: StateKey,
        failed_revision: SafeU64,
        seen_revision: Option<SafeU64>,
    },
    StateAck {
        key: StateKey,
        revision: SafeU64,
    },
    RootEnd {
        root: RootId,
    },
    InvalidatePrincipal {
        principal: Value,
        origin: String,
        through_generation: SafeU64,
    },
}

#[derive(Debug)]
pub(crate) enum DecodeError {
    Fatal(RuntimeError),
    RecoverableDelta {
        key: StateKey,
        failed_revision: u64,
        error: Box<RuntimeError>,
    },
}

pub(crate) fn decode(bytes: &[u8], maximum: usize) -> std::result::Result<Frame, DecodeError> {
    let fail = |message| DecodeError::Fatal(RuntimeError::invalid("rpc_frame", message));
    if bytes.len() > maximum.min(MAX_FRAME) {
        return Err(fail("frame exceeds byte limit".into()));
    }
    let text = std::str::from_utf8(bytes).map_err(|error| fail(error.to_string()))?;
    let value = tembra_contracts::parse_json(text).map_err(|error| fail(error.to_string()))?;
    match serde_json::from_value(value.clone()) {
        Ok(frame) => Ok(frame),
        Err(error) => {
            let recovery = (|| {
                let object = value.as_object()?;
                if object.get("t")?.as_str()? != "delta"
                    || object.keys().map(String::as_str).collect::<BTreeSet<_>>()
                        != BTreeSet::from(["t", "key", "batch"])
                {
                    return None;
                }
                let key: StateKey = serde_json::from_value(object.get("key")?.clone()).ok()?;
                let batch = object.get("batch")?.as_object()?;
                if batch.keys().map(String::as_str).collect::<BTreeSet<_>>()
                    != BTreeSet::from(["base_revision", "result_revision", "ops"])
                {
                    return None;
                }
                let revision: SafeU64 =
                    serde_json::from_value(batch.get("result_revision")?.clone()).ok()?;
                Some((key, revision.get()))
            })();
            if let Some((key, failed_revision)) = recovery {
                Err(DecodeError::RecoverableDelta {
                    key,
                    failed_revision,
                    error: Box::new(RuntimeError::invalid("delta", error.to_string())),
                })
            } else {
                Err(fail(error.to_string()))
            }
        }
    }
}

pub(crate) fn encode(frame: &Frame, maximum: usize) -> Result<Rc<[u8]>> {
    let text = serde_json::to_string(frame)
        .map_err(|error| RuntimeError::invalid("rpc_frame", error.to_string()))?;
    if text.len() > maximum.min(MAX_FRAME) {
        return Err(RuntimeError::invalid(
            "frame_limit",
            "encoded frame exceeds byte limit",
        ));
    }
    // Validate the complete envelope, including nesting added around valid local payloads.
    tembra_contracts::parse_json(&text)
        .map_err(|error| RuntimeError::invalid("frame_profile", error.to_string()))?;
    Ok(Rc::from(text.into_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn wire_presence_and_strict_fields_preserve_null_and_absence() {
        for payload in [
            Payload::Absent,
            Payload::Present(Value::Null),
            Payload::Present(json!([])),
        ] {
            let frame = Frame::Call {
                call: CallId::new(1).unwrap(),
                hold: HoldId::new(1).unwrap(),
                member: "value".into(),
                args: payload.clone(),
            };
            let encoded = encode(&frame, MAX_FRAME).unwrap();
            let Frame::Call { args, .. } = decode(&encoded, MAX_FRAME).unwrap() else {
                panic!("wrong frame")
            };
            assert_eq!(args, payload);
        }
        for bytes in [
            concat!(
                r#"{"t":"call","call":1,"hold":1,"member":"x","#,
                r#""args":{"presence":"absent","value":null}}"#,
            )
            .as_bytes(),
            concat!(
                r#"{"t":"call","call":0,"hold":1,"member":"x","#,
                r#""args":{"presence":"absent"}}"#,
            )
            .as_bytes(),
            concat!(
                r#"{"t":"call","call":1,"hold":1,"member":"x","#,
                r#""args":{"presence":"present"}}"#,
            )
            .as_bytes(),
            concat!(
                r#"{"t":"call","call":1,"hold":1,"member":"x","#,
                r#""args":{"presence":"absent"},"root":{}}"#,
            )
            .as_bytes(),
            br#"{"t":"ping","nonce":1,"nonce":2}"#.as_slice(),
        ] {
            assert!(
                matches!(decode(bytes, MAX_FRAME), Err(DecodeError::Fatal(_))),
                "accepted {}",
                String::from_utf8_lossy(bytes)
            );
        }
    }

    #[test]
    fn unit_authority_modes_and_client_hello_reject_extra_fields() {
        assert!(
            serde_json::from_value::<BindingMode>(
                json!({"mode":"direct","consumer_fingerprint":"forged"})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<BindingMode>(
                json!({"mode":"adapted","consumer_fingerprint":"known","principal":"forged"})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<PeerHello>(json!({"role":"client","code_version":1})).is_err()
        );
        assert!(matches!(
            serde_json::from_value::<BindingMode>(json!({"mode":"direct"})).unwrap(),
            BindingMode::Direct
        ));
    }

    #[test]
    fn delta_recovery_requires_checked_exact_header_and_revision() {
        let value = json!({
            "t": "delta",
            "key": { "hold": 1, "unit": 1, "member": "count", "stream": 1 },
            "batch": {
                "base_revision": 0,
                "result_revision": 1,
                "ops": [{ "op": "remove", "path": [] }],
            },
        });
        let bytes = serde_json::to_vec(&value).unwrap();
        assert!(matches!(
            decode(&bytes, MAX_FRAME),
            Err(DecodeError::RecoverableDelta {
                failed_revision: 1,
                ..
            })
        ));
        for path in ["key", "batch"] {
            let mut invalid = value.clone();
            invalid[path]["unknown"] = json!(1);
            assert!(matches!(
                decode(&serde_json::to_vec(&invalid).unwrap(), MAX_FRAME),
                Err(DecodeError::Fatal(_))
            ));
        }
        let mut invalid = value;
        invalid["batch"]["result_revision"] = json!(9_007_199_254_740_992_u64);
        assert!(matches!(
            decode(&serde_json::to_vec(&invalid).unwrap(), MAX_FRAME),
            Err(DecodeError::Fatal(_))
        ));
    }

    #[test]
    fn outbound_envelope_is_checked_before_admission_not_only_its_payload() {
        let mut value = Value::Null;
        for _ in 0..128 {
            value = Value::Array(vec![value]);
        }
        let local = serde_json::to_string(&value).unwrap();
        assert!(tembra_contracts::parse_json(&local).is_ok());
        let frame = Frame::Call {
            call: CallId::new(1).unwrap(),
            hold: HoldId::new(1).unwrap(),
            member: "x".into(),
            args: Payload::Present(value),
        };
        assert_eq!(
            encode(&frame, MAX_FRAME).unwrap_err().reason,
            "frame_profile"
        );
    }
}
