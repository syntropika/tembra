//! Managed Rust facets over the language-neutral Tembra kernel.
pub mod codec;
mod facet;
mod host;
mod state;

pub use facet::{Facet, Service};
pub use host::{Context, Declaration, Host, Reference, Resource, ResourceKey, ResourceSignal};
pub use state::State;
pub use tembra_core as core;
pub use tembra_core::{Code, Error, Result};
