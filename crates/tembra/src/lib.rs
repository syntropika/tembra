//! Managed Rust orbs over the language-neutral Tembra kernel.
pub mod codec;
mod host;
mod orb;
mod state;

pub use host::{Context, Declaration, Host, Reference, Resource, ResourceKey, ResourceSignal};
pub use orb::{Orb, Service};
pub use state::State;
pub use tembra_core as core;
pub use tembra_core::{Code, Error, Result};
