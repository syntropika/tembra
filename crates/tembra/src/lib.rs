//! Managed Rust orbs over the language-neutral Tembra kernel.
mod clock;
pub mod codec;
#[cfg(feature = "hooks")]
mod hooks;
mod host;
mod orb;
mod state;
pub use clock::Clock;
#[cfg(feature = "subscriptions")]
mod events;
mod readable;
#[cfg(feature = "subscriptions")]
pub use events::{Detach, EventHandler, EventOptions, EventSource};
pub use readable::{Observation, Readable};

#[cfg(feature = "hooks")]
pub use hooks::{Hook, HookHandle};
pub use host::{
    AcquisitionOptions, Cancellation, Context, Declaration, Host, Reference, Resource, ResourceKey,
    ResourceSignal, TaskScope,
};
pub use orb::{Orb, Service};
pub use state::State;
pub use tembra_core as core;
pub use tembra_core::{Code, Error, Result};
