//! Managed Rust orbs over the language-neutral Tembra kernel.
mod clock;
pub mod codec;
#[cfg(feature = "hooks")]
mod hooks;
mod host;
mod orb;
pub mod portable;
mod state;
pub use clock::Clock;
#[cfg(feature = "subscriptions")]
mod events;
mod readable;
#[cfg(feature = "rpc")]
pub mod rpc;
#[cfg(feature = "subscriptions")]
pub use events::{Detach, EventHandler, EventOptions, EventSource};
pub use readable::{Observation, Readable};

#[cfg(feature = "hooks")]
pub use hooks::{Hook, HookHandle};
pub use host::{
    AcquisitionOptions, Cancellation, Context, Declaration, ForeignNotification, Host, IssuedCall,
    LogRecord, Reference, Resource, ResourceKey, ResourceSignal, TaskScope,
};
pub use orb::{Codec, Orb, Service};
/// The JSON crate the codecs `#[tembra::orb]` generates read and write portable values with: an
/// Orb crate names it through the facade, with no dependency of its own.
#[doc(hidden)]
pub use serde_json;
pub use state::State;
pub use tembra_core as core;
pub use tembra_core::{Code, Error, Result};
