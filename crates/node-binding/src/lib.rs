//! Thread-affine metadata-only Node-API adapter. JavaScript stays in the existing Node runtime.
use napi_derive::napi;
mod interpreter;

/// Immutable Orb bytes and their checked prepared contract, shared by every installation.
#[napi]
pub struct NodeArtifact {
    pub(crate) artifact: tembra_contracts::Artifact,
}

#[napi]
impl NodeArtifact {
    /// Parse Wasm data without creating a guest or evaluating implementation code.
    #[napi(constructor)]
    pub fn new(bytes: napi::bindgen_prelude::Uint8Array) -> napi::Result<Self> {
        Ok(Self {
            artifact: tembra_contracts::Artifact::decode(&bytes)
                .map_err(|error| napi::Error::from_reason(error.to_string()))?,
        })
    }

    /// Read the one identity carried by the artifact.
    #[napi]
    pub fn description(&self) -> String {
        self.artifact.describe().to_string()
    }

    /// Bind one prepared signature before invoking any executable callback.
    #[napi]
    pub fn bind(&self, binding: String, method: String, exports: bool) -> napi::Result<u32> {
        self.artifact
            .contract
            .bind(&binding, &method, exports)
            .map_err(|error| napi::Error::from_reason(error.to_string()))
    }

    /// Decode only the portable value; control metadata is an installed signature slot.
    #[napi]
    pub fn validate(&self, slot: u32, arguments: bool, value: String) -> String {
        self.artifact
            .contract
            .validate_packet(slot, arguments, &value)
    }

    /// Reuse this artifact's prepared contract for installation queries.
    #[napi]
    pub fn request(&self, packet: String) -> String {
        self.artifact.contract.request(&packet)
    }
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

    /// Check a fixed scalar scope without deserializing a JavaScript object.
    #[napi(ts_return_type = "string | null")]
    pub fn check(&self, root: f64, call: Option<f64>) -> Option<String> {
        self.kernel.check(root, call)
    }

    /// Execute one closed kernel operation on structured input and return `{ ok }` or `{ error }`.
    ///
    /// No JSON text crosses this seam; the bridge decodes the object strictly within its budget.
    #[napi]
    pub fn command<'env>(
        &self,
        env: &napi::Env,
        op: String,
        input: napi::bindgen_prelude::Object<'env>,
    ) -> napi::Result<napi::bindgen_prelude::Unknown<'env>> {
        let outcome = self.kernel.outcome(&op, &mut napi::De::new(&input));
        env.to_js_value(&outcome)
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
    /// Package a closed source bundle for the selected world.
    #[napi]
    pub fn source_artifact(
        &self,
        source: String,
        name: String,
        api_version: u32,
    ) -> napi::Result<napi::bindgen_prelude::Buffer> {
        self.contract
            .source_artifact(&source, &name, api_version)
            .map(Into::into)
            .map_err(|error| napi::Error::from_reason(error.to_string()))
    }

    /// Package a checked component with its WIT-derived contribution roles.
    #[napi]
    pub fn component_artifact(
        &self,
        component: napi::bindgen_prelude::Uint8Array,
        name: String,
        api_version: u32,
        contributes: Option<std::collections::HashMap<String, f64>>,
    ) -> napi::Result<napi::bindgen_prelude::Buffer> {
        let contributions = contributes
            .unwrap_or_default()
            .into_iter()
            .map(|(binding, priority)| {
                if !priority.is_finite()
                    || priority.fract() != 0.0
                    || priority < f64::from(i32::MIN)
                    || priority > f64::from(i32::MAX)
                {
                    return Err(napi::Error::from_reason(
                        "contribution priority must be a signed 32-bit integer",
                    ));
                }
                Ok((binding, priority as i32))
            })
            .collect::<napi::Result<_>>()?;
        let roles = tembra_contracts::CallbackRoles {
            contributions,
            ..Default::default()
        };
        self.contract
            .component_artifact(&component, &name, api_version, &roles)
            .map(Into::into)
            .map_err(|error| napi::Error::from_reason(error.to_string()))
    }
    /// Resolve a WIT directory and its `deps/` once, keeping each import's provenance, so
    /// type-only dependencies never require or grant callable authority.
    #[napi(constructor)]
    pub fn new(directory: String, world: Option<String>) -> napi::Result<Self> {
        Ok(Self {
            contract: tembra_contracts::Contract::from_dir(directory, world.as_deref())
                .map_err(|error| napi::Error::from_reason(error.to_string()))?,
        })
    }

    /// The standard encoded WIT package, for type generation only; it carries no authority.
    #[napi]
    pub fn wit(&self) -> napi::Result<napi::bindgen_prelude::Buffer> {
        self.contract
            .encode()
            .map(Into::into)
            .map_err(|error| napi::Error::from_reason(error.to_string()))
    }
    /// Inspect and install through the shared prepared-contract implementation.
    #[napi]
    pub fn request(&self, packet: String) -> String {
        self.contract.request(&packet)
    }
}
