//! A bounded, presence-aware RPC protocol shared by native and browser adapters.
//!
//! Admission into a Link is not execution acknowledgement. Lost admitted calls have
//! unknown outcomes and are never replayed by this module. Provider work remains
//! driver-owned until physical settlement after cancellation or logical link closure.

mod connection;
mod consumer_linger;
mod memory;
pub(crate) mod protocol;
pub(crate) mod reload;
pub(crate) mod remote;
pub(crate) mod roots;

pub use connection::{
    Connection, ConnectionOptions, ConnectionProfile, ConnectionStatus, RpcLimits,
};
pub use consumer_linger::ConsumerLingerOptions;
pub use memory::{MemoryControl, MemoryLinkOptions, memory_link};
pub use protocol::{ConnectionId, RootOrigin};
pub use reload::{ClusterReloadReport, ParticipantReloadReport, ReloadFacetLoader, RemoteFacet};

use std::rc::Rc;

use futures::stream::LocalBoxStream;

use crate::RuntimeError;

/// One bounded local transport endpoint. Messages remain raw until Rust decoding.
pub struct Link {
    pub tx: Rc<dyn LinkTx>,
    pub events: LocalBoxStream<'static, LinkEvent>,
}

/// Atomic local admission; implementations must never invoke their peer synchronously.
pub trait LinkTx {
    /// Admits the complete message or no bytes of it. Success is not remote receipt.
    ///
    /// # Errors
    /// Returns the precise local admission refusal; retrying WouldBlock is safe.
    fn try_send(&self, utf8: Rc<[u8]>) -> std::result::Result<(), SendError>;
    fn close(&self, reason: CloseReason);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendError {
    WouldBlock,
    Closed,
    TooLarge,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CloseReason {
    pub error: RuntimeError,
}

impl CloseReason {
    pub fn new(error: RuntimeError) -> Self {
        Self { error }
    }

    pub fn closed() -> Self {
        Self::new(RuntimeError::unavailable("closed", "connection closed"))
    }
}

/// FIFO transport events, ending with exactly one Closed event.
#[derive(Clone, Debug)]
pub enum LinkEvent {
    Opened,
    Message(Vec<u8>),
    Writable,
    Closed(CloseReason),
}
