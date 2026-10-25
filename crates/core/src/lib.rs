//! A language-neutral kernel for facets, managed objects and versioned operations.
//!
//! The kernel stores opaque value and callback identities. The adapter that issued an identity
//! owns its value, performs validation, and executes user code outside kernel borrows.

mod error;
#[cfg(feature = "hooks")]
mod hooks;
mod host;
mod index;
mod model;
mod program;
mod state;
#[cfg(feature = "subscriptions")]
mod subscriptions;
#[cfg(feature = "telemetry")]
pub mod telemetry;

pub use error::{Code, Error, Reason, Result};
#[cfg(feature = "hooks")]
pub use hooks::*;
pub use host::Host;
pub use model::*;
pub use program::{Completion, Effect, Replacement, Scope, Step};
pub use state::{PreparedState, Snapshot};
#[cfg(feature = "subscriptions")]
pub use subscriptions::*;
