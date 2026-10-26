//! Typed context values. Rust owns propagation and frame lifetime.

mod frames;
mod key;
mod value;

use crate::{Error, Reason, Result};
pub(crate) use frames::ContextFrame;
pub use key::{ContextContract, ContextDatum, ContextKey, ContextKind};
pub use value::{ContextValue, OpaqueContextValue};

/// Report an invalid context contract or frame.
fn invalid(message: &str) -> Error {
    Error::invalid(Reason::Context, message)
}
