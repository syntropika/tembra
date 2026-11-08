//! Own immutable Orb data and its resolved contract before creating any executable guest.
use crate::{Contract, Error};
use serde::{Deserialize, Serialize};
use std::{borrow::Cow, collections::BTreeMap};
use wasm_encoder::{CustomSection, Encode, Section};
use wasmparser::{Parser, Payload};

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

/// The parsed contract and payload stay together; platform bindings expose an opaque handle.
pub struct Artifact {
    pub identity: Identity,
    pub contract: Contract,
    pub implementation: Implementation,
}

impl Contract {
    /// Package checked source or component bytes without evaluating the author's implementation.
    pub fn artifact(
        &self,
        source: Option<&str>,
        component: Option<&[u8]>,
        name: &str,
        api_version: u32,
    ) -> Result<Vec<u8>, Error> {
        let identity = Identity {
            name: name.into(),
            api_version,
            world: self.resolve.worlds[self.world].name.clone(),
        };
        identity.validate()?;
        if source.is_some() == component.is_some() {
            return Err(Error(
                "an artifact requires exactly one source bundle or component".into(),
            ));
        }
        let contract = self.encode()?;
        let mut bytes = if let Some(component) = component {
            if !sections(component)?.is_empty() {
                return Err(Error(
                    "component already contains Orb artifact metadata".into(),
                ));
            }
            self.check_component_exports(component)?;
            component.to_vec()
        } else {
            self.check_value_profile()?;
            contract.clone()
        };
        let mut append = |name: &'static str, data: &[u8]| {
            let section = CustomSection {
                name: Cow::Borrowed(name),
                data: Cow::Borrowed(data),
            };
            bytes.push(section.id());
            section.encode(&mut bytes);
        };
        if component.is_some() {
            append("tembra:contract", &contract);
        }
        append(
            "tembra:orb",
            &serde_json::to_vec(&identity).map_err(|error| Error(error.to_string()))?,
        );
        if let Some(source) = source {
            append("tembra:source", source.as_bytes());
        }
        Ok(bytes)
    }
}

impl Identity {
    /// Refuse malformed registration metadata on both packaging and loading paths.
    fn validate(&self) -> Result<(), Error> {
        if self.name.trim().is_empty() || self.api_version == 0 || self.world.trim().is_empty() {
            return Err(Error(
                "artifact identity requires a name, world and positive API version".into(),
            ));
        }
        Ok(())
    }
}

impl Artifact {
    /// Decode immutable data once and check the real component imports and exports before linking.
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let mut sections = sections(bytes)?;
        let identity: Identity = serde_json::from_slice(
            &sections
                .remove("tembra:orb")
                .ok_or_else(|| Error("missing artifact identity".into()))?,
        )
        .map_err(|error| Error(error.to_string()))?;
        identity.validate()?;
        let source = sections
            .remove("tembra:source")
            .map(String::from_utf8)
            .transpose()
            .map_err(|error| Error(error.to_string()))?;
        let (contract, implementation) = if let Some(source) = source {
            // The WIT package is the source artifact's body, so no second copy is embedded.
            let contract = Contract::from_bytes(bytes, Some(&identity.world))?;
            contract.check_value_profile()?;
            (contract, Implementation::Source(source))
        } else {
            let encoded = sections
                .remove("tembra:contract")
                .ok_or_else(|| Error("missing component artifact contract".into()))?;
            let contract = Contract::from_bytes(&encoded, Some(&identity.world))?;
            contract.check_component_exports(bytes)?;
            (contract, Implementation::Component(bytes.to_vec()))
        };
        if !sections.is_empty() {
            return Err(Error("unsupported artifact metadata section".into()));
        }
        Ok(Self {
            identity,
            contract,
            implementation,
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

/// Read only the containing component's metadata and reject duplicate descriptions.
fn sections(bytes: &[u8]) -> Result<BTreeMap<String, Vec<u8>>, Error> {
    let mut sections = BTreeMap::new();
    let mut depth = 0;
    for payload in Parser::new(0).parse_all(bytes) {
        match payload.map_err(|error| Error(error.to_string()))? {
            Payload::Version { .. } => depth += 1,
            Payload::End(_) => depth -= 1,
            Payload::CustomSection(section)
                if depth == 1 && section.name().starts_with("tembra:") =>
            {
                let previous = sections.insert(section.name().to_owned(), section.data().to_vec());
                if previous.is_some() {
                    return Err(Error(format!(
                        "duplicate artifact section {}",
                        section.name()
                    )));
                }
            }
            _ => {}
        }
    }
    Ok(sections)
}
