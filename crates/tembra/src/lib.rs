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

/// Expand the input only when this crate links the native runtime, through its `host` feature.
///
/// `#[tembra::orb]` wraps its native output in it: `orb()`, the clients and every other item
/// that names the runtime, such as `Orb` or `Service`. Invoke it in item position with those
/// items as its input, through the absolute path, since the generated module can hold a
/// `tembra` module of WIT bindings that shadows this crate:
///
/// ```text
/// ::tembra::__host! {
///     pub fn orb() -> ::tembra::Result<::tembra::Orb> { /* .. */ }
/// }
/// ```
///
/// The macro reads this crate's features, not the caller's, and Cargo unifies them across a
/// build. A native host that depends on an Orb crate enables `host`, so the items exist. A
/// component build of the same crate enables only `macros`, so the macro expands to nothing and
/// the runtime is not linked.
#[cfg(feature = "host")]
#[doc(hidden)]
#[macro_export]
macro_rules! __host {
    ($($item:tt)*) => {
        $($item)*
    };
}

/// Without `host`, the input is dropped; the other definition describes the macro.
#[cfg(not(feature = "host"))]
#[doc(hidden)]
#[macro_export]
macro_rules! __host {
    ($($item:tt)*) => {};
}

// A build without `host` compiles this only because the macro drops it: `Orb` exists only with
// `host`.
__host! {
    const _: Option<Orb> = None;
}
