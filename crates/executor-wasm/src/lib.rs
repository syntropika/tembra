//! Isolated interpreters execute callbacks while the existing host owns Orb lifecycle.
mod adapter;
mod bindings;
mod component;
mod engine;
mod port;
mod prepared;
mod providers;
mod worker;

pub use component::ComponentPort;
pub use engine::{Engine, Limits, LogRecord};
pub use port::{InterpreterPort, PortEvent, PortEvents, PortReply, PortTicket};
pub use prepared::PreparedSource;
pub use providers::{FilesystemGrant, Grants, NetworkGrant};
pub use tembra_contracts::{Binding, Contract};

use tembra::{Error, core::Code};

/// Preserve the executor boundary in diagnostics returned through the native facade.
fn failure(message: impl ToString) -> Error {
    Error::new(Code::Unavailable, "wasm_executor", message.to_string())
}
