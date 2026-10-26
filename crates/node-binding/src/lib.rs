//! Thread-affine metadata-only Node-API adapter. JavaScript stays in the existing Node runtime.
use napi_derive::napi;

#[napi]
pub struct NodeKernel {
    kernel: tembra_bridge::Kernel,
}

#[napi]
impl NodeKernel {
    /// Create one kernel and its metadata bridge.
    #[napi(constructor)]
    pub fn new() -> Self {
        Self {
            kernel: tembra_bridge::Kernel::new(),
        }
    }

    /// Execute one metadata request without retaining language values.
    #[napi]
    pub fn request(&self, metadata: String) -> String {
        self.kernel.request(&metadata)
    }
}

impl Default for NodeKernel {
    /// Create a binding with a fresh kernel.
    fn default() -> Self {
        Self::new()
    }
}
