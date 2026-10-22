//! Atomic portable state with weak capabilities and synchronous derivations.
//!
//! Mutations validate private candidates before adoption. Callback effects outside the
//! graph are not rolled back. Native callback panics propagate; an observer panic can
//! occur after adoption and interrupt remaining delivery. Ordinary observer errors are
//! reported independently of a successful mutation.

//! Portable immutable state with graph-local preparation and FIFO publication.
//!
//! Canonical state JSON is limited to 1 MiB of UTF-8 and the shared parser's
//! 128-container nesting limit. These limits also apply to derived values.
//! Independent external writes are not rolled back by a failed local preparation.
//! Their projections refresh after preparation; following a native panic, queued
//! refreshes resume on the next non-unwinding graph operation.

mod cell;
mod graph;
mod port;
mod publication;

#[cfg(test)]
mod tests;

use std::rc::Rc;

use serde_json::Value;

use crate::{Result, RuntimeError, delta::DeltaBatch};

pub use cell::{Cell, Prepared, ReplaceOptions, StateDeclaration, StateMetadataChange};
pub(crate) use graph::{
    CandidateSources, Exposure, GraphDeclarationSet, GraphReload, PreparedGraphReload,
    ReadyBarrier, StateGraph,
};
pub(crate) use port::ReadPort;
pub use port::{Readable, Subscription};
pub(crate) use publication::PublicationQueue;

pub(crate) type ReadGuard = Rc<dyn Fn() -> Result<()>>;
pub(crate) type WriteGuard = Rc<dyn Fn(&crate::Context) -> Result<()>>;
pub(crate) type Reporter = Rc<dyn Fn(RuntimeError)>;

const MAX_REVISION: u64 = 9_007_199_254_740_991;

fn gone() -> RuntimeError {
    RuntimeError::new(
        crate::ErrorCode::Gone,
        "state",
        "state authority no longer exists",
    )
}

fn pending() -> RuntimeError {
    RuntimeError::new(
        crate::ErrorCode::NotReady,
        "state",
        "state has not been initialized",
    )
}

// Floating representations and Serde's reserved number map can be observable
// under arbitrary_precision. Keep the original parser path for those values.
fn needs_json_roundtrip(value: &Value) -> bool {
    match value {
        Value::Number(number) => !number.is_i64() && !number.is_u64(),
        Value::Array(values) => values.iter().any(needs_json_roundtrip),
        Value::Object(values) => {
            values.contains_key("$serde_json::private::Number")
                || values.values().any(needs_json_roundtrip)
        }
        _ => false,
    }
}

fn normalize(value: &Value) -> Result<Rc<Value>> {
    if needs_json_roundtrip(value) {
        let encoded = tembra_contracts::canonical_json(value)
            .map_err(|error| RuntimeError::invalid(error.reason, error.message))?;
        return tembra_contracts::parse_json(&encoded)
            .map(Rc::new)
            .map_err(|error| RuntimeError::invalid(error.reason, error.message));
    }
    tembra_contracts::from_portable_value::<Value>(value)
        .map(Rc::new)
        .map_err(|error| RuntimeError::invalid(error.reason, error.message))
}

/// An immutable portable value and its node's safe-integer revision.
#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    value: Rc<Value>,
    revision: u64,
}

impl Snapshot {
    pub(crate) fn new(value: Rc<Value>, revision: u64) -> Result<Self> {
        if revision > MAX_REVISION {
            return Err(RuntimeError::invalid(
                "state_revision",
                "revision exceeds safe integer range",
            ));
        }
        Ok(Self {
            value: normalize(&value)?,
            revision,
        })
    }

    pub(super) fn normalized(value: Rc<Value>, revision: u64) -> Self {
        Self { value, revision }
    }

    pub fn value(&self) -> &Value {
        &self.value
    }

    pub fn shared_value(&self) -> Rc<Value> {
        self.value.clone()
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }
}

/// A first snapshot, revision-checked operations, or an authoritative stream reset.
#[derive(Clone, Debug, PartialEq)]
pub enum StateEvent {
    Snapshot(Snapshot),
    Ops {
        delta: DeltaBatch,
        snapshot: Snapshot,
    },
    Reset(Snapshot),
}

impl StateEvent {
    pub fn snapshot(&self) -> &Snapshot {
        match self {
            Self::Snapshot(snapshot) | Self::Reset(snapshot) | Self::Ops { snapshot, .. } => {
                snapshot
            }
        }
    }
}
