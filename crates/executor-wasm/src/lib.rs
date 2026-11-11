//! Isolated interpreters execute callbacks while the existing host owns Orb lifecycle.
/// Shared with the guest, which uses the encoder/decoder halves the host does not.
#[allow(dead_code)]
mod abi;
mod adapter;
mod bindings;
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
// The build script's generator qualification rules are unit-tested with the host.
#[cfg(test)]
#[allow(dead_code)]
#[path = "../codegen/mod.rs"]
mod codegen;

pub use engine::{Engine, Limits, LogRecord};
pub use install::{Cli, Input, Install, Output, ReaderFactory, WriterFactory};
#[doc(hidden)]
pub use port::{InterpreterPort, PortEvent, PortEvents, PortReply, PortTicket};
pub use providers::{
    Deterministic, FilesystemGrant, Grants, HostMonotonicClock, HostWallClock, HttpGrant,
    NetworkGrant, Rng, SystemCapability,
};
#[doc(hidden)]
pub use tembra_contracts::{Binding, ComponentRoles, Contract};

use tembra::{Error, core::Code};

/// Preserve the executor boundary in diagnostics returned through the native facade.
fn failure(message: impl ToString) -> Error {
    Error::new(Code::Unavailable, "wasm_executor", message.to_string())
}
