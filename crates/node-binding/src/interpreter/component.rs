//! Compiled components share the same facade; their dedicated Wasmtime worker settles a threadsafe deferred.
use super::{NodeInterpreter, envelope, js_error};
use napi_derive::napi;
use serde::Deserialize;
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc};
use tembra_executor_wasm::{Binding, ComponentPort, Grants};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Registration {
    #[serde(default)]
    bindings: BTreeMap<String, Binding>,
    #[serde(default)]
    grants: Grants,
}

#[napi]
impl NodeInterpreter {
    /// Resolve a compiled artifact's WIT and grants before publishing any service callbacks.
    #[napi]
    pub fn open_component(
        &self,
        artifact: &crate::NodeArtifact,
        configuration: String,
    ) -> napi::Result<NodeComponentPort> {
        let config: Registration = serde_json::from_str(&configuration).map_err(js_error)?;
        let port = self
            .engine
            .open_component(&artifact.artifact, &config.bindings, &config.grants)
            .map_err(js_error)?;
        Ok(NodeComponentPort {
            port: Some(Arc::new(port)),
        })
    }
}

/// A call retains its physical Store independently of the JavaScript observer's lifetime.
#[napi]
pub struct NodeComponentPort {
    port: Option<Arc<ComponentPort>>,
}

#[napi]
impl NodeComponentPort {
    /// Read the already validated metadata plan without executing component functions.
    #[napi]
    pub fn description(&self) -> napi::Result<String> {
        Ok(self
            .port
            .as_ref()
            .ok_or_else(|| js_error("component is closed"))?
            .description()
            .to_string())
    }

    /// Invoke through the real Canonical ABI without occupying a libuv worker while the call waits.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn call<'env>(
        &self,
        env: &'env napi::Env,
        interface: String,
        method: String,
        arguments: String,
    ) -> napi::Result<napi::bindgen_prelude::Object<'env>> {
        let port = self
            .port
            .as_ref()
            .ok_or_else(|| js_error("component is closed"))?
            .clone();
        let arguments: Value = serde_json::from_str(&arguments).map_err(js_error)?;
        let (deferred, promise) = env.create_deferred::<String, _>()?;
        let owner = port.clone();
        port.submit(&interface, &method, arguments, move |result| {
            let packet = envelope(result).to_string();
            deferred.resolve(move |_| {
                // Release this call's claim on the JavaScript thread; the worker cannot join itself.
                drop(owner);
                Ok::<String, napi::Error>(packet)
            });
        });
        Ok(promise)
    }

    /// Release only the facade's claim; each submitted call retains its own endpoint until it settles.
    #[napi]
    pub fn close(&mut self) {
        self.port.take();
    }
}
