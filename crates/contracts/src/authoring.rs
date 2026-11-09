//! WIT resolution, packaging and real compiled-component verification for native tooling.
//!
//! This module is the only user of `wit-parser` and `wit-component`. Runtime queries are answered
//! by the prepared contract it produces, so tooling and runtimes share one implementation.
use crate::{
    Artifact, Error, PreparedContract,
    artifact::{Container, Identity, append_section, encode_orb_contributions},
};
use std::path::Path;
use wit_parser::{Resolve, WorldId};

/// A resolved package graph with an explicitly selected entry world.
#[derive(Clone)]
pub struct Contract {
    pub(crate) resolve: Resolve,
    pub(crate) world: WorldId,
    prepared: PreparedContract,
}

/// Every runtime query is answered by the prepared contract.
impl std::ops::Deref for Contract {
    type Target = PreparedContract;

    fn deref(&self) -> &PreparedContract {
        &self.prepared
    }
}

/// Format parser diagnostics with their full context.
fn parser(error: impl std::fmt::Display) -> Error {
    Error(format!("{error:#}"))
}

impl Contract {
    /// Prepare the compact runtime contract once for a resolved world.
    pub(crate) fn new(resolve: Resolve, world: WorldId) -> Result<Self, Error> {
        let prepared = crate::prepare::world(&resolve, world)?;
        Ok(Self {
            resolve,
            world,
            prepared,
        })
    }

    /// Resolve a WIT directory and its local dependencies without acquiring packages.
    pub fn from_dir(path: impl AsRef<Path>, world: Option<&str>) -> Result<Self, Error> {
        let mut resolve = crate::standard::resolve()?;
        let (package, _) = resolve.push_dir(path).map_err(parser)?;
        let world = resolve.select_world(&[package], world).map_err(parser)?;
        Self::new(resolve, world)
    }

    /// Resolve one self-contained WIT source and require selection for ambiguous worlds.
    pub fn from_source(name: &str, source: &str, world: Option<&str>) -> Result<Self, Error> {
        let mut resolve = crate::standard::resolve()?;
        let package = resolve.push_str(name, source).map_err(parser)?;
        let world = resolve.select_world(&[package], world).map_err(parser)?;
        Self::new(resolve, world)
    }

    /// Decode an encoded WIT package produced by `encode`.
    pub fn from_bytes(bytes: &[u8], world: Option<&str>) -> Result<Self, Error> {
        let wit_component::DecodedWasm::WitPackage(resolve, package) =
            wit_component::decode(bytes).map_err(parser)?
        else {
            return Err(Error(
                "prepared contracts require an encoded WIT package".into(),
            ));
        };
        let world = resolve.select_world(&[package], world).map_err(parser)?;
        Self::new(resolve, world)
    }

    /// The compact runtime contract embedded in artifacts.
    pub fn prepared(&self) -> &PreparedContract {
        &self.prepared
    }

    /// Encode the resolved package and standard dependencies for in-memory binding generation.
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        let package = self.resolve.worlds[self.world]
            .package
            .ok_or_else(|| Error("selected world has no package".into()))?;
        wit_component::encode(&self.resolve, package, false).map_err(parser)
    }

    /// Require a compiled component's exported interfaces and signatures to match this world.
    pub fn check_component_exports(&self, bytes: &[u8]) -> Result<(), Error> {
        verify_component(&self.prepared, bytes)
    }

    /// Compare complete exported signatures independently of resolver-local type indexes.
    pub fn check_exports(&self, other: &Self) -> Result<(), Error> {
        if self.signatures(true)? != other.signatures(true)? {
            return Err(Error(
                "component export signatures differ from the selected WIT world".into(),
            ));
        }
        Ok(())
    }

    /// Package a closed source bundle for the selected world.
    pub fn source_artifact(
        &self,
        source: &str,
        name: &str,
        api_version: u32,
    ) -> Result<Vec<u8>, Error> {
        Artifact::package_source(name, api_version, &self.prepared, source)
    }

    /// Package a component with typed contribution roles; WIT owns every callback signature.
    pub fn component_artifact(
        &self,
        component: &[u8],
        name: &str,
        api_version: u32,
        contributions: &std::collections::BTreeMap<String, i32>,
    ) -> Result<Vec<u8>, Error> {
        self.prepared.check_contribution_roles(contributions)?;
        let identity = Identity {
            name: name.into(),
            api_version,
            world: self.prepared.world().to_owned(),
        };
        identity.validate()?;
        if !Container::parse(component)?.tembra.is_empty() {
            return Err(Error(
                "component already contains Orb artifact metadata".into(),
            ));
        }
        self.check_component_exports(component)?;
        let mut bytes = component.to_vec();
        append_section(
            &mut bytes,
            "tembra:orb",
            &encode_orb_contributions(&identity, &self.prepared, contributions),
        );
        Ok(bytes)
    }
}

/// Require exact exports and check every retained executable import against its declared signature.
pub(crate) fn verify_component(expected: &PreparedContract, bytes: &[u8]) -> Result<(), Error> {
    let wit_component::DecodedWasm::Component(resolve, world) =
        wit_component::decode(bytes).map_err(parser)?
    else {
        return Err(Error("expected an executable WebAssembly component".into()));
    };
    let actual = crate::prepare::world(&resolve, world)?;
    if expected.signatures(true)? != actual.signatures(true)? {
        return Err(Error(
            "component export signatures differ from the selected WIT world".into(),
        ));
    }
    if expected.used_import_signatures(&actual)? != actual.signatures(false)? {
        return Err(Error(
            "component import signatures differ from the selected WIT world".into(),
        ));
    }
    Ok(())
}
