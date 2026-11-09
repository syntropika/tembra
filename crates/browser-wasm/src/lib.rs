//! Browser adapter for metadata commands. No JavaScript engine, loader, callbacks or values.
//!
//! Inputs arrive as structured JavaScript objects and results leave as objects; there is no JSON
//! text on this seam. The bridge decodes inputs strictly and within its metadata budget.

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
pub struct WasmKernel {
    kernel: tembra_bridge::Kernel,
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
impl WasmKernel {
    /// Create one kernel and its metadata bridge.
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen(constructor))]
    pub fn new() -> Self {
        Self {
            kernel: tembra_bridge::Kernel::new(),
        }
    }

    /// Check an issued invocation through a fixed scalar entry, without an object visitor.
    pub fn check(&self, root: f64, call: Option<f64>) -> Option<String> {
        self.kernel.check(root, call)
    }

    /// Execute one closed kernel operation and return `{ ok }` or `{ error }`.
    #[cfg(target_arch = "wasm32")]
    pub fn command(&self, op: &str, input: JsValue) -> JsValue {
        use serde::Serialize;
        let serializer = serde_wasm_bindgen::Serializer::new()
            .serialize_maps_as_objects(true)
            .serialize_missing_as_null(true);
        let outcome = self
            .kernel
            .outcome(op, serde_wasm_bindgen::Deserializer::from(input));
        outcome.serialize(&serializer).unwrap_or_else(|error| {
            let error = tembra_bridge::core::Error::invalid(
                tembra_bridge::core::Reason::Serialization,
                error.to_string(),
            );
            tembra_bridge::Outcome::Error(error.into())
                .serialize(&serializer)
                .unwrap_or(JsValue::NULL)
        })
    }
}

impl WasmKernel {
    /// Borrow the bridge for typed Rust callers sharing this binding.
    pub fn kernel(&self) -> &tembra_bridge::Kernel {
        &self.kernel
    }
}

impl Default for WasmKernel {
    /// Create a binding with a fresh kernel.
    fn default() -> Self {
        Self::new()
    }
}
