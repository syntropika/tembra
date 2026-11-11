//! Isolated component Stores execute callbacks while the existing host owns Orb lifecycle.
mod adapter;
mod bindings;
mod clocks;
mod component;
mod dns;
mod engine;
mod http;
pub mod ingress;
mod install;
mod io;
mod port;
mod protocol;
mod providers;
mod sockets;
mod stdio;
mod worker;

pub use engine::{Engine, Limits, LogRecord};
pub use install::{Cli, Input, Install, Output, ReaderFactory, WriterFactory};
#[doc(hidden)]
pub use port::{InterpreterPort, PortEvent, PortEvents, PortReply, PortTicket};
pub use providers::{
    Deterministic, FilesystemGrant, Grants, HostMonotonicClock, HostWallClock, HttpGrant,
    NetworkGrant, Rng, SystemCapability,
};
#[doc(hidden)]
pub use tembra_contracts::{Binding, Contract};

use tembra::{Error, core::Code};

/// Preserve the executor boundary in diagnostics returned through the native facade.
fn failure(message: impl ToString) -> Error {
    Error::new(Code::Unavailable, "wasm_executor", message.to_string())
}
