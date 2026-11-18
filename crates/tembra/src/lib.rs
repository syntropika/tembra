//! Declare, install and call Rust and isolated WebAssembly Orbs through one crate.
//!
//! The default `host` feature runs native Orbs. Enable `wasm` on native targets for
//! `wasm::Engine`, which loads isolated Orbs as the same `Orb` type.
//! Enable `contracts` for prepared WIT contracts or `authoring` for WIT parsing and packaging.
//! Guests use `default-features = false` with `macros` to declare components using `orb`
//! without linking the host runtime; the feature brings the `wit-bindgen` the generated
//! bindings use, so a guest never names it.
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

// The bindings generator the macro's expansion calls, and the runtime of the bindings it writes:
// the generated code follows the rules of this version, so the facade picks it, not the guest.
#[cfg(feature = "macros")]
#[doc(hidden)]
pub use wit_bindgen as __wit_bindgen;

/// Isolated WebAssembly execution, providers and prepared WIT contracts.
#[cfg(all(feature = "wasm", not(target_arch = "wasm32")))]
pub use tembra_executor_wasm as wasm;

/// Expand the input only when this crate links the native runtime, through its `host` feature,
/// and builds for a target other than wasm32.
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
/// the runtime is not linked. A build for wasm32 is a component whatever the features, as the
/// macro's `export!` is, so the macro expands to nothing there too: a guest whose `tembra` keeps
/// the default features, or gets `host` from another crate of the build, stays a component.
#[cfg(all(feature = "host", not(target_arch = "wasm32")))]
#[doc(hidden)]
#[macro_export]
macro_rules! __host {
    ($($item:tt)*) => {
        $($item)*
    };
}

/// Without `host`, or for wasm32, the input is dropped; the other definition describes the macro.
#[cfg(any(not(feature = "host"), target_arch = "wasm32"))]
#[doc(hidden)]
#[macro_export]
macro_rules! __host {
    ($($item:tt)*) => {};
}

// A build without `host` compiles this only because the macro drops it: `Orb` exists only with
// `host`. A build for wasm32 drops it too, whatever the features.
__host! {
    const _: Option<Orb> = None;
    const _: () = assert!(!cfg!(target_arch = "wasm32"));
}

/// Expand the input only where `__host!` drops its own: the counterpart of `__host!`, for an item
/// a component build has and a native host replaces with its own, of the same name, from
/// `__host!`.
///
/// `#[tembra::orb]` re-exports the bindings' `context` module through it, by the absolute path, so
/// a native host gets the `context` the native side writes in its place.
#[cfg(any(not(feature = "host"), target_arch = "wasm32"))]
#[doc(hidden)]
#[macro_export]
macro_rules! __guest {
    ($($item:tt)*) => {
        $($item)*
    };
}

/// With `host`, outside wasm32, the input is dropped; the other definition describes the macro.
#[cfg(all(feature = "host", not(target_arch = "wasm32")))]
#[doc(hidden)]
#[macro_export]
macro_rules! __guest {
    ($($item:tt)*) => {};
}

// A build with `host`, outside wasm32, compiles this only because the macro drops it.
__guest! {
    const _: () = assert!(!cfg!(feature = "host") || cfg!(target_arch = "wasm32"));
}

/// Expand the input only when this crate does not link std, without its `host` feature.
///
/// `#[tembra::orb(no_std)]` wraps the panic handler of its runtime prelude in it, by the absolute
/// path: with `host`, this crate links std, whose panic handler a second one would duplicate, on
/// wasm32 too. The macro reads this crate's features, which Cargo unifies across a build.
#[cfg(not(feature = "host"))]
#[doc(hidden)]
#[macro_export]
macro_rules! __freestanding {
    ($($item:tt)*) => {
        $($item)*
    };
}

/// With `host`, the input is dropped; the other definition describes the macro.
#[cfg(feature = "host")]
#[doc(hidden)]
#[macro_export]
macro_rules! __freestanding {
    ($($item:tt)*) => {};
}

// A build with `host` compiles this only because the macro drops it.
__freestanding! {
    const _: () = assert!(!cfg!(feature = "host"));
}

/// Expand the first braced input when this crate links the runtime's hooks, through its `hooks`
/// feature, and the second otherwise: `::tembra::__hooks! { { with } { without } }`, in
/// expression position, each input an expression.
///
/// The native side of `#[tembra::orb]` adds a contribution to a hook through it, by the absolute
/// path: `Orb::contribute_portable` exists only with `hooks`, so without it the contribution
/// answers an error naming the feature, and the module still compiles. The macro reads this
/// crate's features, which Cargo unifies across a build.
#[cfg(feature = "hooks")]
#[doc(hidden)]
#[macro_export]
macro_rules! __hooks {
    ({ $($with:tt)* } { $($without:tt)* }) => {
        $($with)*
    };
}

/// Without `hooks`, the second input; the other definition describes the macro.
#[cfg(not(feature = "hooks"))]
#[doc(hidden)]
#[macro_export]
macro_rules! __hooks {
    ({ $($with:tt)* } { $($without:tt)* }) => {
        $($without)*
    };
}

// Each build selects the input of its features.
const _: () = assert!(__hooks! { { cfg!(feature = "hooks") } { !cfg!(feature = "hooks") } });
