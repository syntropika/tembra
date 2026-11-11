//! Browser binding for prepared Orb artifacts, separate from the small lifecycle kernel.
//!
//! Every artifact is a WebAssembly component. Construction runs the same verifier as native
//! hosts: a compiled component is checked against its public contract, and a source component
//! (carrying `tembra:runtime`) is split into its public contract, the private `tembra:orb`
//! package, hook self-imports and the fixed runtime internals, each checked against the actual
//! component types. Neither embedded metadata nor a claimed hash is trusted.
use tembra_contracts::Artifact;

mod limits;
pub use limits::{InstrumentedCore, instrument_core};
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

/// Immutable verified artifact data shared by one installed revision.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
pub struct WasmArtifact {
    artifact: Artifact,
    imports: Vec<String>,
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
impl WasmArtifact {
    /// Decode and verify the actual component before a browser creates any guest authority.
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen(constructor))]
    pub fn new(bytes: &[u8]) -> Result<WasmArtifact, Failure> {
        let artifact = Artifact::decode(bytes).map_err(failure)?;
        let imports = component_imports(&artifact.component).map_err(failure)?;
        Ok(Self { artifact, imports })
    }

    /// Read identity without copying the implementation through JSON.
    pub fn description(&self) -> String {
        self.artifact.describe().to_string()
    }

    /// Whether the verified component exports the private `tembra:orb` lifecycle.
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = sourceRuntime))]
    pub fn source_runtime(&self) -> bool {
        self.artifact.source_runtime
    }

    /// Copy the verified component bytes once for the trusted glue generator.
    pub fn component(&self) -> Vec<u8> {
        self.artifact.component.clone()
    }

    /// The exact versioned top-level import names of the verified component, as a JSON array.
    pub fn imports(&self) -> String {
        serde_json::Value::from(self.imports.clone()).to_string()
    }

    /// A compiled component's packaged contribution roles (`{binding: priority}`) as JSON. Source
    /// components describe theirs through `lifecycle.describe`, so this is empty for them.
    pub fn contributions(&self) -> String {
        serde_json::to_string(&self.artifact.contributions).expect("serializable contributions")
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
}

/// Read the outer component's import names; nested components and core modules are skipped.
fn component_imports(bytes: &[u8]) -> Result<Vec<String>, String> {
    let mut names = Vec::new();
    let mut depth = 0usize;
    for payload in wasmparser::Parser::new(0).parse_all(bytes) {
        match payload.map_err(|error| error.to_string())? {
            wasmparser::Payload::ModuleSection { .. }
            | wasmparser::Payload::ComponentSection { .. } => depth += 1,
            wasmparser::Payload::End(_) => depth = depth.saturating_sub(1),
            wasmparser::Payload::ComponentImportSection(reader) if depth == 0 => {
                for import in reader {
                    let import = import.map_err(|error| error.to_string())?;
                    names.push(import.name.name.to_owned());
                }
            }
            _ => {}
        }
    }
    Ok(names)
}

#[cfg(test)]
mod tests;
