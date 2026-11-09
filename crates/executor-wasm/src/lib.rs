//! Isolated interpreters execute callbacks while the existing host owns Orb lifecycle.
/// Shared with the guest, which uses the encoder/decoder halves the host does not.
#[allow(dead_code)]
mod abi;
mod adapter;
mod bindings;
mod component;
mod engine;
mod install;
mod io;
mod port;
mod protocol;
mod providers;
mod worker;

#[doc(hidden)]
pub use component::ComponentPort;
pub use engine::{Engine, Limits, LogRecord};
pub use install::Install;
#[doc(hidden)]
pub use io::IoOperation;
#[doc(hidden)]
pub use port::{InterpreterPort, PortEvent, PortEvents, PortReply, PortTicket};
pub use providers::{FilesystemGrant, Grants, NetworkGrant};
#[doc(hidden)]
pub use tembra_contracts::{Binding, Contract};

use tembra::{Error, core::Code};

/// Preserve the executor boundary in diagnostics returned through the native facade.
fn failure(message: impl ToString) -> Error {
    Error::new(Code::Unavailable, "wasm_executor", message.to_string())
}
