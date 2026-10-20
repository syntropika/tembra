//! Thread-affine native execution and bounded text WebSocket transport.
//!
//! Drive [`TokioDriver::run_until`] inside a Tokio current-thread runtime. The
//! adapter carries bytes; shared `tembra_core::rpc` owns protocol and authority.

pub mod counter;

mod driver;
mod websocket;

pub use driver::{TokioDriver, random_incarnation};
pub use websocket::{WebSocketLink, WebSocketOptions};
