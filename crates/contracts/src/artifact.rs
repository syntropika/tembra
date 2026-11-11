//! Own immutable Orb data and its prepared contract before creating any executable guest.
//!
//! An artifact is a WebAssembly component container. Source Orbs are an empty component carrying
//! only `tembra:orb` (identity and compact contract) and `tembra:source`. Compiled Orbs append
//! `tembra:orb` to the real component, whose actual WIT types are verified against the contract
//! by the native `authoring` verifier before any linking.
use crate::{
    Error, HookComposition, Lifetime, Manifest, PreparedContract, ServicePolicy, codec::Reader,
    model::MAX_NAME,
};
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
    /// What a compiled component declares beyond its contract: its hook roles and the lifetime
    /// of its services. A source bundle declares these in executable code, so its manifest is
    /// empty.
    pub manifest: Manifest,
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
        contract.check_value_profile()?;
        let mut bytes = COMPONENT_HEADER.to_vec();
        append_section(
            &mut bytes,
            ORB,
            &encode_orb(&identity, contract, &Manifest::default()),
        );
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
        let (identity, contract, manifest) = decode_orb(orb)?;
        let implementation = if let Some(source) = source {
            if manifest != Manifest::default() {
                return Err(Error(
                    "source artifacts declare their hook roles and service lifetimes in their bundle"
                        .into(),
                ));
            }
            if bytes[..8] != COMPONENT_HEADER || container.other {
                return Err(Error(
                    "source artifacts may only contain Orb metadata and source".into(),
                ));
            }
            contract.check_value_profile()?;
            Implementation::Source(
                String::from_utf8(source.to_vec()).map_err(|error| Error(error.to_string()))?,
            )
        } else {
            verify_component(&contract, bytes)?;
            contract.check_manifest(&manifest)?;
            Implementation::Component(bytes.to_vec())
        };
        Ok(Self {
            identity,
            contract,
            implementation,
            manifest,
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

/// Encode identity, contract and manifest into one compact `tembra:orb` payload: the
/// contributions with their priorities, then the declared hooks with their composition, then the
/// service policies with their lifetime and linger.
pub(crate) fn encode_orb(
    identity: &Identity,
    contract: &PreparedContract,
    manifest: &Manifest,
) -> Vec<u8> {
    let mut writer = crate::codec::Writer::default();
    writer.str(&identity.name);
    writer.u32(identity.api_version);
    writer.bytes(&contract.encode());
    writer.len(manifest.contributions.len());
    for (binding, priority) in &manifest.contributions {
        writer.str(binding);
        writer.u32(u32::from_le_bytes(priority.to_le_bytes()));
    }
    writer.len(manifest.hooks.len());
    for (binding, composition) in &manifest.hooks {
        writer.str(binding);
        writer.byte(match composition {
            HookComposition::Series => 0,
            HookComposition::Waterfall => 1,
            HookComposition::Bail => 2,
        });
    }
    writer.len(manifest.services.len());
    for (identity, policy) in &manifest.services {
        writer.str(identity);
        writer.byte(match policy.lifetime {
            Lifetime::Referenced => 0,
            Lifetime::Host => 1,
        });
        writer.u32(policy.linger_ms);
    }
    writer.finish()
}

/// Decode and validate identity, contract and manifest from untrusted bytes.
fn decode_orb(bytes: &[u8]) -> Result<(Identity, PreparedContract, Manifest), Error> {
    let mut reader = Reader::new(bytes);
    let name = reader.str()?;
    let api_version = reader.u32()?;
    let length = reader.u32()? as usize;
    let contract = PreparedContract::decode(reader.take(length)?)?;
    let mut manifest = Manifest::default();
    for _ in 0..reader.count(crate::model::MAX_ITEMS)? {
        let binding = reader.str()?;
        let priority = i32::from_le_bytes(reader.u32()?.to_le_bytes());
        if manifest.contributions.insert(binding, priority).is_some() {
            return Err(Error("duplicate compiled contribution role".into()));
        }
    }
    for _ in 0..reader.count(crate::model::MAX_ITEMS)? {
        let binding = reader.str()?;
        let composition = match reader.byte()? {
            0 => HookComposition::Series,
            1 => HookComposition::Waterfall,
            2 => HookComposition::Bail,
            _ => return Err(Error("unknown compiled hook composition".into())),
        };
        if manifest.hooks.insert(binding, composition).is_some() {
            return Err(Error("duplicate compiled hook declaration".into()));
        }
    }
    for _ in 0..reader.count(crate::model::MAX_ITEMS)? {
        let identity = reader.str()?;
        let lifetime = match reader.byte()? {
            0 => Lifetime::Referenced,
            1 => Lifetime::Host,
            _ => return Err(Error("unknown compiled service lifetime".into())),
        };
        let linger_ms = reader.u32()?;
        let policy = ServicePolicy {
            lifetime,
            linger_ms,
        };
        if manifest.services.insert(identity, policy).is_some() {
            return Err(Error("duplicate compiled service policy".into()));
        }
    }
    reader.finish()?;
    let identity = Identity {
        name,
        api_version,
        world: contract.world().to_owned(),
    };
    identity.validate()?;
    Ok((identity, contract, manifest))
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
