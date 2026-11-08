//! Thread-affine metadata-only Node-API adapter. JavaScript stays in the existing Node runtime.
use napi_derive::napi;
mod interpreter;

/// Resolve canonical WIT dependencies for build tooling without evaluating Orb implementations.
#[napi]
pub fn compile_contract(
    directory: String,
    world: Option<String>,
) -> napi::Result<napi::bindgen_prelude::Buffer> {
    let contract = tembra_contracts::Contract::from_dir(directory, world.as_deref())
        .map_err(|error| napi::Error::from_reason(error.to_string()))?;
    Ok(contract
        .encode()
        .map_err(|error| napi::Error::from_reason(error.to_string()))?
        .into())
}

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

/// A resolved contract shared by optional interpreted-Orb platform facades.
#[napi]
pub struct NodeContract {
    contract: tembra_contracts::Contract,
}

#[napi]
impl NodeContract {
    /// Decode a prepared package without reading project files at runtime.
    #[napi(constructor)]
    pub fn new(
        bytes: napi::bindgen_prelude::Uint8Array,
        world: Option<String>,
    ) -> napi::Result<Self> {
        Ok(Self {
            contract: tembra_contracts::Contract::from_bytes(&bytes, world.as_deref())
                .map_err(|error| napi::Error::from_reason(error.to_string()))?,
        })
    }
    /// Derive metadata or validate portable values through the shared WIT implementation.
    #[napi]
    pub fn request(&self, packet: String) -> String {
        self.contract.request(&packet)
    }
}
