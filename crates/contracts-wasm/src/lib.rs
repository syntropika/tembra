//! Optional contract queries are separate from the small browser lifecycle kernel.
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

/// Immutable artifact data shares one resolved contract across isolated installations.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
pub struct WasmArtifact {
    artifact: tembra_contracts::Artifact,
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
impl WasmArtifact {
    /// Parse Wasm data before a browser creates any guest authority.
    #[wasm_bindgen(constructor)]
    pub fn new(bytes: &[u8]) -> Result<WasmArtifact, JsValue> {
        Ok(Self {
            artifact: tembra_contracts::Artifact::decode(bytes)
                .map_err(|error| JsValue::from_str(&error.to_string()))?,
        })
    }
    /// Read identity without copying the implementation through JSON.
    pub fn description(&self) -> String {
        self.artifact.describe().to_string()
    }
    /// Reuse the same contract queries as native installations.
    pub fn request(&self, packet: &str) -> String {
        self.artifact.contract.request(packet)
    }
    /// Copy source once when creating a fresh interpreter heap.
    pub fn source(&self) -> Result<String, JsValue> {
        match &self.artifact.implementation {
            tembra_contracts::ArtifactImplementation::Source(source) => Ok(source.clone()),
            _ => Err(JsValue::from_str(
                "compiled Orb execution is unavailable in this browser backend",
            )),
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl WasmArtifact {
    /// Keep native checks connected to the actual artifact implementation.
    pub fn description(&self) -> String {
        self.artifact.describe().to_string()
    }
}
