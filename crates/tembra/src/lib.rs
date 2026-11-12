//! Declare, install and call Rust and isolated WebAssembly Orbs through one crate.
//!
//! The default `host` feature runs native Orbs. Enable `wasm` on native targets for
//! `wasm::Engine`, which loads isolated Orbs as the same `Orb` type.
//! Enable `contracts` for prepared WIT contracts or `authoring` for WIT parsing and packaging.
//! Guests use `default-features = false` with `macros` to declare components using `orb`
//! without linking the host runtime.
//! Browser applications use the TypeScript SDK and its WebAssembly host adapter.
#![cfg_attr(not(feature = "host"), no_std)]

#[cfg(feature = "host")]
pub use tembra_runtime::*;

/// Prepared WIT contracts, Orb artifacts and installation grants.
#[cfg(feature = "contracts")]
pub use tembra_contracts as contracts;

/// Declare a compiled Orb and its WIT bindings from a Rust module.
#[cfg(feature = "macros")]
pub use tembra_orb_macro::orb;

/// Isolated WebAssembly execution, providers and prepared WIT contracts.
#[cfg(all(feature = "wasm", not(target_arch = "wasm32")))]
pub use tembra_executor_wasm as wasm;
