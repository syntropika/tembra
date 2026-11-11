//! Browser binding for prepared Orb contracts, separate from the small lifecycle kernel.
//!
//! This crate links only the runtime contract model. [`WasmArtifact`] refuses compiled Orb
//! artifacts because their real component types cannot be verified without the native verifier,
//! which is deliberately not shipped here. A compiled Orb is accepted only as a
//! [`WasmPreparedComponent`]: the SDK constructs one solely from its registry of host-trusted
//! prepared adapters, and the constructor itself binds the actual bytes to an adapter digest.
use tembra_contracts::{Artifact, ArtifactImplementation, PreparedComponent};
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

/// Whether `bytes` claim a compiled component (Orb metadata without a source bundle).
///
/// A cheap container scan that selects the browser execution path; it grants nothing.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = compiledArtifact))]
pub fn compiled_artifact(bytes: &[u8]) -> bool {
    Artifact::is_compiled(bytes)
}

/// Metadata of a compiled Orb whose exact bytes a host-trusted prepared adapter was built from.
///
/// Native tooling verified the component's real WIT types against this contract before it
/// generated the adapter; the browser only re-checks the binding by digest. It answers the same
/// install-time queries as [`WasmArtifact`] and carries no executable payload.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
pub struct WasmPreparedComponent {
    component: PreparedComponent,
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
impl WasmPreparedComponent {
    /// Hash `bytes` and decode their metadata only when the digest is one of `digests`, the
    /// SHA-256 digests of the application's registered prepared adapters.
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen(constructor))]
    pub fn new(bytes: &[u8], digests: Vec<String>) -> Result<WasmPreparedComponent, Failure> {
        Ok(Self {
            component: PreparedComponent::decode(bytes, &digests).map_err(failure)?,
        })
    }

    /// The lowercase hexadecimal SHA-256 digest that selected the adapter.
    pub fn digest(&self) -> String {
        self.component.digest.clone()
    }

    /// Read identity as `WasmArtifact.description` does, with kind `component`.
    pub fn description(&self) -> String {
        self.component.describe().to_string()
    }

    /// Compiled contribution roles and their priorities, as a JSON object.
    pub fn contributions(&self) -> String {
        self.component.describe_contributions().to_string()
    }

    /// Declared hooks and their composition (`series`, `waterfall`, `bail`), as a JSON object.
    pub fn hooks(&self) -> String {
        self.component.describe_hooks().to_string()
    }

    /// Service policies (`lifetime`, `linger_ms`) by exported interface, as a JSON object.
    pub fn services(&self) -> String {
        self.component.describe_services().to_string()
    }

    /// Answer the typed install-time queries: `install`, `implementation` and `describe`.
    pub fn request(&self, packet: &str) -> String {
        self.component.contract.request(packet)
    }

    /// Bind an export (by interface identity) or import (by world binding) method once.
    pub fn bind(&self, binding: &str, method: &str, exports: bool) -> Result<u32, Failure> {
        self.component
            .contract
            .bind(binding, method, exports)
            .map_err(failure)
    }

    /// Validate a JSON argument tuple or result through a bound slot; returns the ok/error envelope.
    pub fn validate(&self, slot: u32, arguments: bool, value: &str) -> String {
        self.component
            .contract
            .validate_packet(slot, arguments, value)
    }
}

#[cfg(test)]
mod tests;
