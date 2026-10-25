//! Browser adapter for metadata commands. No JavaScript engine, loader, callbacks or values.

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
pub struct WasmKernel {
    kernel: tembra_bridge::Kernel,
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
impl WasmKernel {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen(constructor))]
    pub fn new() -> Self {
        Self {
            kernel: tembra_bridge::Kernel::new(),
        }
    }

    pub fn request(&self, metadata: &str) -> String {
        self.kernel.request(metadata)
    }
}

impl Default for WasmKernel {
    fn default() -> Self {
        Self::new()
    }
}
