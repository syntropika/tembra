//! Browser binding for prepared Orb contracts, separate from the small lifecycle kernel.
//!
//! This crate links only the runtime contract model. Compiled Orb artifacts are refused at
//! construction because their real component types cannot be verified without the native
//! verifier, which is deliberately not shipped here.
use tembra_contracts::{Artifact, ArtifactImplementation};
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

/// Errors cross to JavaScript as values in browsers and stay strings in native tests.
#[cfg(target_arch = "wasm32")]
type Failure = JsValue;
#[cfg(not(target_arch = "wasm32"))]
type Failure = String;

/// Convert a contract diagnostic for the current platform.
fn failure(error: impl std::fmt::Display) -> Failure {
    #[cfg(target_arch = "wasm32")]
    return JsValue::from_str(&error.to_string());
    #[cfg(not(target_arch = "wasm32"))]
    return error.to_string();
}

/// Immutable artifact data shares one prepared contract across isolated installations.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
pub struct WasmArtifact {
    artifact: Artifact,
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
impl WasmArtifact {
    /// Decode and re-validate the prepared contract before a browser creates any guest authority.
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen(constructor))]
    pub fn new(bytes: &[u8]) -> Result<WasmArtifact, Failure> {
        Ok(Self {
            artifact: Artifact::decode(bytes).map_err(failure)?,
        })
    }

    /// Read identity without copying the implementation through JSON.
    pub fn description(&self) -> String {
        self.artifact.describe().to_string()
    }

    /// Answer the typed install-time queries: `install`, `implementation` and `describe`.
    pub fn request(&self, packet: &str) -> String {
        self.artifact.contract.request(packet)
    }

    /// Bind an export (by interface identity) or import (by world binding) method once.
    pub fn bind(&self, binding: &str, method: &str, exports: bool) -> Result<u32, Failure> {
        self.artifact
            .contract
            .bind(binding, method, exports)
            .map_err(failure)
    }

    /// Validate a JSON argument tuple or result through a bound slot; returns the ok/error envelope.
    pub fn validate(&self, slot: u32, arguments: bool, value: &str) -> String {
        self.artifact
            .contract
            .validate_packet(slot, arguments, value)
    }

    /// Copy source once when creating a fresh interpreter heap.
    pub fn source(&self) -> Result<String, Failure> {
        match &self.artifact.implementation {
            ArtifactImplementation::Source(source) => Ok(source.clone()),
            ArtifactImplementation::Component(_) => Err(failure(
                "compiled Orb execution is unavailable in this browser backend",
            )),
        }
    }
}

#[cfg(test)]
mod tests;
