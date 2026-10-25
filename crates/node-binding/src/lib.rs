//! Thread-affine metadata-only Node-API adapter. JavaScript stays in the existing Node runtime.
use napi_derive::napi;

#[napi]
pub struct NodeKernel {
    kernel: tembra_bridge::Kernel,
}

#[napi]
impl NodeKernel {
    #[napi(constructor)]
    pub fn new() -> Self {
        Self {
            kernel: tembra_bridge::Kernel::new(),
        }
    }

    #[napi]
    pub fn request(&self, metadata: String) -> String {
        self.kernel.request(&metadata)
    }
}

impl Default for NodeKernel {
    fn default() -> Self {
        Self::new()
    }
}
