//! Optional contract queries are separate from the small browser lifecycle kernel.
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

/// A resolved WIT package with the same planning and validation implementation on every host.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
pub struct WasmContract {
    contract: tembra_contracts::Contract,
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
impl WasmContract {
    /// Decode a closed preparation artifact without acquiring files or dependencies.
    #[wasm_bindgen(constructor)]
    pub fn new(bytes: &[u8], world: Option<String>) -> Result<Self, JsValue> {
        Ok(Self {
            contract: tembra_contracts::Contract::from_bytes(bytes, world.as_deref())
                .map_err(|error| JsValue::from_str(&error.to_string()))?,
        })
    }

    /// Execute the shared data-only contract query protocol.
    pub fn request(&self, packet: &str) -> String {
        self.contract.request(packet)
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl WasmContract {
    /// Keep native build checks useful without exporting a second platform API.
    pub fn request(&self, packet: &str) -> String {
        self.contract.request(packet)
    }
}
