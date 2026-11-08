//! Portable prepared source carries its WIT package, not generated implementation bindings.
use crate::{Binding, Contract, Engine, Grants, failure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use tembra::{Orb, Result};

/// A closed dependency bundle produced by Tembra's optional TypeScript preparation tooling.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PreparedSource {
    pub format: String,
    pub source: String,
    pub sha256: String,
    pub types_sha256: String,
    pub contract: Vec<u8>,
    pub world: Option<String>,
}

impl Engine {
    /// Verify prepared source identity and resolve the embedded contract before granting capabilities.
    pub fn prepare_source(
        &self,
        name: &str,
        api_version: u32,
        prepared: &PreparedSource,
        bindings: &BTreeMap<String, Binding>,
        grants: &Grants,
    ) -> Result<Orb> {
        let contract = prepared.resolve()?;
        self.prepare_with_grants(
            &contract,
            name,
            api_version,
            &prepared.source,
            bindings,
            grants,
        )
    }
}

impl PreparedSource {
    /// Check the closed bundle identity before decoding its embedded WIT package.
    pub fn resolve(&self) -> Result<Contract> {
        if self.format != "tembra-interpreter-source-v1" {
            return Err(failure("unsupported prepared source format"));
        }
        let digest = format!("{:x}", Sha256::digest(self.source.as_bytes()));
        if digest != self.sha256 {
            return Err(failure(
                "prepared source digest does not match its closed bundle",
            ));
        }
        Contract::from_bytes(&self.contract, self.world.as_deref()).map_err(failure)
    }
}
