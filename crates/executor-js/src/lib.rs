//! Single-thread-affine JavaScript execution over one existing Tembra Host.
//!
//! Artifacts are compiled, import-free ESM. The adapter owns JavaScript callbacks and
//! physical promise jobs; core declarations, identity, state and cancellation stay in core.
//! Application extensions are trusted. This adapter does not establish a hostile-code sandbox.
mod artifact;
mod binding;
mod engine;
mod native;
mod options;
mod scope;
mod slots;

pub use artifact::BundledJsArtifact;
pub use engine::Executor;
pub use native::{
    DeferredNativeCallableFn, NativeCall, NativeCallControl, NativeCallable, NativeCallableFn,
    NativeDecoder, NativeMethodCodec, NativeMethodSignature, NativeRecord, NativeValue,
    ScopedCodec,
};
pub use options::{ExecutorOptions, ExecutorStats};

mod hooks;
