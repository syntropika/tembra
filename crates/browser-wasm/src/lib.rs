//! Browser scheduling and capability bindings around the shared Rust runtime.
//!
//! JavaScript owns explicit references and subscriptions; core state and RPC remain in Rust.

#![cfg(target_arch = "wasm32")]

mod boundary;
mod driver;
mod executor;
mod intrinsics;
mod ipc;
mod options;
mod readable;
mod runtime;
mod socket;

pub use boundary::*;
pub use driver::BrowserDriver;
pub use executor::*;
pub use readable::WasmSubscription;
pub use runtime::{WasmConnection, WasmHost};
pub use socket::{BrowserLinkOptions, websocket_link};
