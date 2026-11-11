//! Own immutable Orb data and its prepared contract before creating any executable guest.
//!
//! An artifact is a real WebAssembly component with `tembra:orb` (identity and compact contract)
//! appended. A source Orb built from JavaScript also carries `tembra:runtime`, naming the private
//! package it was built against. The actual WIT types are verified against the contract by the
//! native `authoring` verifier before any linking; metadata is never trusted alone.
use crate::{Error, PreparedContract, codec::Reader, model::MAX_NAME};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Header of an empty WebAssembly component: magic, version 0x0d, layer 1.
pub(crate) const COMPONENT_HEADER: [u8; 8] = *b"\0asm\x0d\0\x01\0";
const ORB: &str = "tembra:orb";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Identity {
    pub name: String,
    pub api_version: u32,
    pub world: String,
}

/// The prepared contract and payload stay together; platform bindings expose an opaque handle.
pub struct Artifact {
    pub identity: Identity,
    pub contract: PreparedContract,
    /// The verified component bytes, including the appended metadata sections.
    pub component: Vec<u8>,
    /// Compiled callback roles; a source Orb describes its contributions through its lifecycle.
    pub contributions: BTreeMap<String, i32>,
    /// Built from JavaScript against [`crate::SOURCE_PACKAGE`]: the component exports
    /// [`crate::LIFECYCLE`], describes its implementation itself and may import the fixed
    /// [`crate::RUNTIME_INTERNALS`].
    pub source_runtime: bool,
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
    /// Decode immutable data once; compiled components are checked against their real types.
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let mut container = Container::parse(bytes)?;
        let orb = container
            .take(ORB)
            .ok_or_else(|| Error("missing artifact identity".into()))?;
        let runtime = container.take(crate::source::RUNTIME_SECTION);
        if !container.tembra.is_empty() {
            return Err(Error("unsupported artifact metadata section".into()));
        }
        let (identity, contract, contributions) = decode_orb(orb)?;
        if let Some(runtime) = runtime {
            if runtime != crate::SOURCE_PACKAGE.as_bytes() {
                return Err(Error(format!(
                    "source component requires runtime package {}",
                    crate::SOURCE_PACKAGE
                )));
            }
            if !contributions.is_empty() {
                return Err(Error(
                    "source components describe their implementation in their lifecycle".into(),
                ));
            }
            verify_source(&contract, bytes)?;
        } else {
            verify_component(&contract, bytes)?;
            contract.check_contribution_roles(&contributions)?;
        }
        Ok(Self {
            identity,
            contract,
            component: bytes.to_vec(),
            contributions,
            source_runtime: runtime.is_some(),
        })
    }

    /// Describe registration data without copying component bytes through JSON.
    pub fn describe(&self) -> serde_json::Value {
        #[derive(Serialize)]
        struct Description<'a> {
            #[serde(flatten)]
            identity: &'a Identity,
            kind: &'static str,
        }
        serde_json::to_value(Description {
            identity: &self.identity,
            kind: if self.source_runtime {
                "source"
            } else {
                "component"
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

/// Check a source component's actual WIT against the public contract and private package.
#[cfg(feature = "authoring")]
fn verify_source(contract: &PreparedContract, bytes: &[u8]) -> Result<(), Error> {
    crate::source::verify_source(contract, bytes)
}

/// Runtimes without the component verifier refuse source components instead of trusting metadata.
#[cfg(not(feature = "authoring"))]
fn verify_source(_: &PreparedContract, _: &[u8]) -> Result<(), Error> {
    Err(Error(
        "source component artifacts require the native component verifier".into(),
    ))
}

/// Encode identity and contract into one compact `tembra:orb` payload.
#[cfg(any(feature = "authoring", test))]
pub(crate) fn encode_orb(identity: &Identity, contract: &PreparedContract) -> Vec<u8> {
    encode_orb_contributions(identity, contract, &BTreeMap::new())
}

#[cfg(any(feature = "authoring", test))]
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
#[cfg(any(feature = "authoring", test))]
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
