//! Declare, install and call Rust and isolated WebAssembly Orbs through one crate.
//!
//! Native Orbs need no optional features. Enable `wasm` on native targets for
//! `wasm::Engine`, which loads isolated Orbs as the same [`Orb`] type.
//! Browser applications use the TypeScript SDK and its WebAssembly host adapter.

pub use tembra_runtime::*;

/// Isolated WebAssembly execution, providers and prepared WIT contracts.
#[cfg(all(feature = "wasm", not(target_arch = "wasm32")))]
pub use tembra_executor_wasm as wasm;
