//! Own immutable Orb data and its prepared contract before creating any executable guest.
//!
//! An artifact is a WebAssembly component container. Source Orbs are an empty component carrying
//! only `tembra:orb` (identity and compact contract) and `tembra:source`. Compiled Orbs append
//! `tembra:orb` to the real component, whose actual WIT types are verified against the contract
//! by the native `authoring` verifier before any linking.
use crate::{Error, PreparedContract, codec::Reader, model::MAX_NAME};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Header of an empty WebAssembly component: magic, version 0x0d, layer 1.
pub(crate) const COMPONENT_HEADER: [u8; 8] = *b"\0asm\x0d\0\x01\0";
const ORB: &str = "tembra:orb";
const SOURCE: &str = "tembra:source";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Identity {
    pub name: String,
    pub api_version: u32,
    pub world: String,
}

/// A closed source bundle or compiled component, never executable host JavaScript.
pub enum Implementation {
    Source(String),
    Component(Vec<u8>),
}

/// The prepared contract and payload stay together; platform bindings expose an opaque handle.
pub struct Artifact {
    pub identity: Identity,
    pub contract: PreparedContract,
    pub implementation: Implementation,
    /// Compiled callback roles; source bundles declare their contributions in executable code.
    pub contributions: BTreeMap<String, i32>,
}

impl Identity {
    /// Refuse malformed registration metadata on both packaging and loading paths.
    pub(crate) fn validate(&self) -> Result<(), Error> {
        if self.name.trim().is_empty()
            || self.name.len() > MAX_NAME
            || self.api_version == 0
            || self.world.trim().is_empty()
        {
            return Err(Error(
                "artifact identity requires a name, world and positive API version".into(),
            ));
        }
        Ok(())
    }
}

impl Artifact {
    /// Package a source Orb for an already prepared contract; no WIT parser is involved.
    pub fn package_source(
        name: &str,
        api_version: u32,
        contract: &PreparedContract,
        source: &str,
    ) -> Result<Vec<u8>, Error> {
        let identity = Identity {
            name: name.into(),
            api_version,
            world: contract.world().to_owned(),
        };
        identity.validate()?;
        contract.check_source_profile()?;
        let mut bytes = COMPONENT_HEADER.to_vec();
        append_section(&mut bytes, ORB, &encode_orb(&identity, contract));
        append_section(&mut bytes, SOURCE, source.as_bytes());
        Ok(bytes)
    }

    /// Decode immutable data once; compiled components are checked against their real types.
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let mut container = Container::parse(bytes)?;
        let orb = container
            .take(ORB)
            .ok_or_else(|| Error("missing artifact identity".into()))?;
        let source = container.take(SOURCE);
        if !container.tembra.is_empty() {
            return Err(Error("unsupported artifact metadata section".into()));
        }
        let (identity, contract, contributions) = decode_orb(orb)?;
        let implementation = if let Some(source) = source {
            if !contributions.is_empty() {
                return Err(Error(
                    "source artifacts declare contributions in their bundle".into(),
                ));
            }
            if bytes[..8] != COMPONENT_HEADER || container.other {
                return Err(Error(
                    "source artifacts may only contain Orb metadata and source".into(),
                ));
            }
            contract.check_source_profile()?;
            Implementation::Source(
                String::from_utf8(source.to_vec()).map_err(|error| Error(error.to_string()))?,
            )
        } else {
            verify_component(&contract, bytes)?;
            contract.check_contribution_roles(&contributions)?;
            Implementation::Component(bytes.to_vec())
        };
        Ok(Self {
            identity,
            contract,
            implementation,
            contributions,
        })
    }

    /// Describe registration data without copying source or component bytes through JSON.
    pub fn describe(&self) -> serde_json::Value {
        #[derive(Serialize)]
        struct Description<'a> {
            #[serde(flatten)]
            identity: &'a Identity,
            kind: &'static str,
        }
        serde_json::to_value(Description {
            identity: &self.identity,
            kind: match self.implementation {
                Implementation::Source(_) => "source",
                Implementation::Component(_) => "component",
            },
        })
        .expect("serializable artifact identity")
    }
}

/// Check a compiled component's actual imports and exports against the prepared contract.
#[cfg(feature = "authoring")]
fn verify_component(contract: &PreparedContract, bytes: &[u8]) -> Result<(), Error> {
    crate::authoring::verify_component(contract, bytes)
}

/// Runtimes without the component verifier refuse compiled Orbs instead of trusting metadata.
#[cfg(not(feature = "authoring"))]
fn verify_component(_: &PreparedContract, _: &[u8]) -> Result<(), Error> {
    Err(Error(
        "compiled Orb artifacts require the native component verifier; this runtime accepts source Orbs only"
            .into(),
    ))
}

/// Encode identity and contract into one compact `tembra:orb` payload.
pub(crate) fn encode_orb(identity: &Identity, contract: &PreparedContract) -> Vec<u8> {
    encode_orb_contributions(identity, contract, &BTreeMap::new())
}

pub(crate) fn encode_orb_contributions(
    identity: &Identity,
    contract: &PreparedContract,
    contributions: &BTreeMap<String, i32>,
) -> Vec<u8> {
    let mut writer = crate::codec::Writer::default();
    writer.str(&identity.name);
    writer.u32(identity.api_version);
    writer.bytes(&contract.encode());
    writer.len(contributions.len());
    for (binding, priority) in contributions {
        writer.str(binding);
        writer.u32(u32::from_le_bytes(priority.to_le_bytes()));
    }
    writer.finish()
}

/// Decode and validate identity and contract from untrusted bytes.
fn decode_orb(bytes: &[u8]) -> Result<(Identity, PreparedContract, BTreeMap<String, i32>), Error> {
    let mut reader = Reader::new(bytes);
    let name = reader.str()?;
    let api_version = reader.u32()?;
    let length = reader.u32()? as usize;
    let contract = PreparedContract::decode(reader.take(length)?)?;
    let mut contributions = BTreeMap::new();
    for _ in 0..reader.count(crate::model::MAX_ITEMS)? {
        let binding = reader.str()?;
        let priority = i32::from_le_bytes(reader.u32()?.to_le_bytes());
        if contributions.insert(binding, priority).is_some() {
            return Err(Error("duplicate compiled contribution role".into()));
        }
    }
    reader.finish()?;
    let identity = Identity {
        name,
        api_version,
        world: contract.world().to_owned(),
    };
    identity.validate()?;
    Ok((identity, contract, contributions))
}

/// Append a custom section to a WebAssembly binary.
pub(crate) fn append_section(bytes: &mut Vec<u8>, name: &str, data: &[u8]) {
    use crate::codec::Writer;
    let mut payload = Writer::default();
    payload.str(name);
    let mut payload = payload.finish();
    payload.extend_from_slice(data);
    let mut header = Writer::default();
    header.byte(0);
    header.len(payload.len());
    bytes.extend_from_slice(&header.finish());
    bytes.extend_from_slice(&payload);
}

/// Top-level sections of a module or component; nested payloads are skipped unread.
pub(crate) struct Container<'a> {
    pub tembra: BTreeMap<&'a str, &'a [u8]>,
    /// Whether any non-Tembra section exists.
    pub other: bool,
}

impl<'a> Container<'a> {
    pub(crate) fn parse(bytes: &'a [u8]) -> Result<Self, Error> {
        let invalid = || Error("artifact is not a WebAssembly binary".into());
        let header = bytes.get(..8).ok_or_else(invalid)?;
        if header != COMPONENT_HEADER && header != b"\0asm\x01\0\0\0" {
            return Err(invalid());
        }
        let mut reader = Reader::new(&bytes[8..]);
        let mut container = Self {
            tembra: BTreeMap::new(),
            other: false,
        };
        while !reader.is_empty() {
            let id = reader.byte()?;
            let size = reader.wasm_u32()? as usize;
            let payload = reader.take(size).map_err(|_| invalid())?;
            if id != 0 {
                container.other = true;
                continue;
            }
            let mut section = Reader::new(payload);
            let length = section.wasm_u32()? as usize;
            let name = std::str::from_utf8(section.take(length).map_err(|_| invalid())?)
                .map_err(|_| invalid())?;
            if !name.starts_with("tembra:") {
                container.other = true;
                continue;
            }
            let data = section.take(payload.len() - section.position())?;
            if container.tembra.insert(name, data).is_some() {
                return Err(Error(format!("duplicate artifact section {name}")));
            }
        }
        Ok(container)
    }

    fn take(&mut self, name: &str) -> Option<&'a [u8]> {
        self.tembra.remove(name)
    }
}
