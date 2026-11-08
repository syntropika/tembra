//! Resolved WIT contracts and explicit provider selection for Orb registration.
mod describe;
mod lower;
mod port;
mod signatures;
mod standard;
mod validate;

use std::{collections::BTreeMap, path::Path};
use wit_parser::{Resolve, WorldId};

pub use lower::{Binding, HostImport, Plan};
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

/// A resolved package graph with an explicitly selected entry world.
#[derive(Clone)]
pub struct Contract {
    resolve: Resolve,
    world: WorldId,
}

impl Contract {
    /// Resolve a WIT directory and its local dependencies without acquiring packages.
    pub fn from_dir(path: impl AsRef<Path>, world: Option<&str>) -> Result<Self, Error> {
        let mut resolve = standard::resolve()?;
        let (package, _) = resolve
            .push_dir(path)
            .map_err(|error| Error(format!("{error:#}")))?;
        let world = resolve
            .select_world(&[package], world)
            .map_err(|error| Error(format!("{error:#}")))?;
        Ok(Self { resolve, world })
    }

    /// Resolve one self-contained WIT source and require selection for ambiguous worlds.
    pub fn from_source(name: &str, source: &str, world: Option<&str>) -> Result<Self, Error> {
        let mut resolve = standard::resolve()?;
        let package = resolve
            .push_str(name, source)
            .map_err(|error| Error(format!("{error:#}")))?;
        let world = resolve
            .select_world(&[package], world)
            .map_err(|error| Error(format!("{error:#}")))?;
        Ok(Self { resolve, world })
    }

    /// Decode a prepared WIT package without reading author files at runtime.
    pub fn from_bytes(bytes: &[u8], world: Option<&str>) -> Result<Self, Error> {
        let wit_component::DecodedWasm::WitPackage(resolve, package) =
            wit_component::decode(bytes).map_err(|error| Error(format!("{error:#}")))?
        else {
            return Err(Error(
                "prepared contracts require an encoded WIT package".into(),
            ));
        };
        let world = resolve
            .select_world(&[package], world)
            .map_err(|error| Error(format!("{error:#}")))?;
        Ok(Self { resolve, world })
    }

    /// Require a compiled component's exported interfaces and signatures to match this world.
    pub fn check_component_exports(&self, bytes: &[u8]) -> Result<(), Error> {
        let wit_component::DecodedWasm::Component(resolve, world) =
            wit_component::decode(bytes).map_err(|error| Error(format!("{error:#}")))?
        else {
            return Err(Error("expected an executable WebAssembly component".into()));
        };
        self.check_exports(&Self { resolve, world })
    }

    /// Compare complete exported signatures independently of resolver-local type indexes.
    fn check_exports(&self, other: &Self) -> Result<(), Error> {
        if signatures::exports(&self.resolve, self.world)?
            != signatures::exports(&other.resolve, other.world)?
        {
            return Err(Error(
                "component export signatures differ from the selected WIT world".into(),
            ));
        }
        Ok(())
    }

    /// Describe complete signatures and the resolved type graph, including resource types.
    pub fn describe(&self) -> serde_json::Value {
        describe::world(&self.resolve, self.world)
    }

    /// Encode the resolved package and standard dependencies for in-memory binding generation.
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        let package = self.resolve.worlds[self.world]
            .package
            .ok_or_else(|| Error("selected world has no package".into()))?;
        wit_component::encode(&self.resolve, package, false)
            .map_err(|error| Error(format!("{error:#}")))
    }

    /// Derive kernel metadata after selecting installed Orb and host providers explicitly.
    pub fn plan(
        &self,
        name: &str,
        api_version: u32,
        bindings: &BTreeMap<String, Binding>,
    ) -> Result<Plan, Error> {
        lower::plan(&self.resolve, self.world, name, api_version, bindings)
    }

    /// Reject signatures that the interpreter's JSON value profile cannot represent faithfully.
    pub fn check_value_profile(&self) -> Result<(), Error> {
        validate::profile(&self.resolve, self.world)
    }

    /// Check a selected Orb import before constructing interpreter reference proxies.
    pub fn check_import_value_profile(&self, binding: &str) -> Result<(), Error> {
        validate::import_profile(&self.resolve, self.world, binding)
    }

    /// Validate a portable argument tuple against its resolved WIT signature.
    pub fn validate_arguments(
        &self,
        interface: &str,
        method: &str,
        arguments: &serde_json::Value,
    ) -> Result<(), Error> {
        validate::arguments(
            &self.resolve,
            self.world,
            interface,
            method,
            arguments,
            true,
        )
    }

    /// Validate a portable return value before it crosses the interpreter boundary.
    pub fn validate_result(
        &self,
        interface: &str,
        method: &str,
        value: &serde_json::Value,
    ) -> Result<(), Error> {
        validate::result(&self.resolve, self.world, interface, method, value, true)
    }

    /// Validate arguments sent through an explicitly selected Orb import binding.
    pub fn validate_import_arguments(
        &self,
        binding: &str,
        method: &str,
        arguments: &serde_json::Value,
    ) -> Result<(), Error> {
        validate::arguments(&self.resolve, self.world, binding, method, arguments, false)
    }

    /// Validate a provider's result before returning it to interpreted code.
    pub fn validate_import_result(
        &self,
        binding: &str,
        method: &str,
        value: &serde_json::Value,
    ) -> Result<(), Error> {
        validate::result(&self.resolve, self.world, binding, method, value, false)
    }
}

/// Qualify a service by its provider and complete, versioned interface identity.
pub fn service_name(provider: &str, contract: &str) -> String {
    format!("{provider}::{contract}")
}

#[cfg(test)]
mod tests;
