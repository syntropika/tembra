//! Prepared WIT contracts and explicit provider selection for Orb registration.
//!
//! Runtimes use [`PreparedContract`], a compact typed model decoded and re-validated from each
//! artifact without a WIT parser. The default `authoring` feature adds [`Contract`], which
//! resolves WIT, packages artifacts and verifies real compiled components.
mod artifact;
#[cfg(feature = "authoring")]
mod authoring;
mod codec;
mod describe;
mod hooks;
mod implementation;
mod lower;
mod model;
mod permissions;
mod port;
#[cfg(feature = "authoring")]
mod prepare;
#[cfg(feature = "authoring")]
mod provenance;
mod resource_profile;
mod selection;
mod signatures;
#[cfg(feature = "authoring")]
mod standard;
mod validate;

pub use artifact::{
    Artifact, Identity, Implementation as ArtifactImplementation, PreparedComponent,
};
#[cfg(feature = "authoring")]
pub use authoring::Contract;
pub use hooks::{ContributionBinding, HookBinding, HookComposition, HookPlan};
pub use implementation::{
    Implementation, IngressBinding, Lifetime, ServiceBinding, ServiceImplementation,
};
pub use lower::{Binding, HostImport, Plan};
pub use model::{
    Case, Entry, Field, Function, FunctionKind, Interface, Item, Kind, Model, PreparedContract,
    ResourceIdentity, Ty,
};
pub use permissions::{FilesystemGrant, Grants, HttpGrant, NetworkGrant, SystemCapability};
pub use port::envelope;
pub use resource_profile::ResourceSignature;
pub use selection::OrbProvider;
pub use validate::javascript_name;

/// A diagnostic produced before an Orb enters the kernel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    /// Display the contract diagnostic without discarding parser context.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

/// Qualify a service by its provider and complete, versioned interface identity.
pub fn service_name(provider: &str, contract: &str) -> String {
    format!("{provider}::{contract}")
}

/// The one standard export served through the host-local incoming HTTP adapter.
///
/// It is recognized by this exact identity only. Its owned `request`/`response` resources never
/// become portable Orb values: the export lowers to no kernel service metadata, no signature and
/// no value-profile slot, and the host's ingress adapter is its only caller.
pub const HTTP_HANDLER: &str = "wasi:http/handler@0.3.0";

#[cfg(all(test, feature = "authoring"))]
mod artifact_tests;
#[cfg(all(test, feature = "authoring"))]
mod conformance_tests;
#[cfg(all(test, feature = "authoring"))]
mod ingress_tests;
#[cfg(test)]
mod permission_tests;
#[cfg(test)]
mod prepared_tests;
#[cfg(all(test, feature = "authoring"))]
mod provenance_tests;
#[cfg(all(test, feature = "authoring"))]
mod resource_identity_tests;
#[cfg(all(test, feature = "authoring"))]
mod resource_profile_tests;
#[cfg(all(test, feature = "authoring"))]
mod tests;
#[cfg(all(test, feature = "authoring"))]
mod wire_tests;
