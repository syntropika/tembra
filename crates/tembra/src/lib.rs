//! Typed contracts, providers and managed references over the shared Tembra core.
//!
//! The facade owns no object table, state graph or execution versions. Generated references
//! keep core claims; cloning them shares a claim and explicit release ends every clone.
//! Use [`core`] for dynamic tooling, transports, executors and advanced lifecycle operations.

extern crate self as tembra;

mod context;
mod declaration;
mod facet;
mod host;
mod state;
mod state_definition;
pub use state_definition::StateDefinition;
mod token;

#[cfg(all(feature = "native", not(target_arch = "wasm32")))]
pub mod native;

pub use context::Context;
pub use core::{
    CancellationToken, Driver, ErrorCode, GetOptions, HostOptions, LoadOptions, ManualDriver,
    RefStatus, Resource, ResourceDeclaration, Result, RuntimeError, ServeOptions,
};
pub use declaration::Declaration;
pub use facet::{FacetDeclaration, NativeFacet};
pub use host::Host;
pub use state::*;
pub use tembra_contracts as contracts;
pub use tembra_core as core;
pub use tembra_macros::contract;
pub use token::{ContractToken, ProviderContract};

// Generated bindings use these paths so applications do not need incidental direct dependencies.
pub use {futures, schemars, serde, serde_json};

/// The common application API; dynamic runtime operations remain accessible through `core()`.
pub mod prelude {
    pub use crate::schemars::JsonSchema;
    pub use crate::serde::{Deserialize, Serialize};
    pub use crate::{
        Cell, Context, ContractToken, Declaration, Host, NativeFacet, Prepared, Readable, Resource,
        ResourceDeclaration, Result, RuntimeError, Snapshot, StateDeclaration, StateDefinition,
        contract,
    };
}
