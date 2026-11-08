//! Compiled components share the same facade; libuv waits while the existing Wasmtime worker executes.
use super::{NodeInterpreter, envelope, js_error};
use napi::{
    Env, Task,
    bindgen_prelude::{AsyncTask, Uint8Array},
};
use napi_derive::napi;
use serde::Deserialize;
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc};
use tembra_executor_wasm::{Binding, ComponentPort, Contract, Grants};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Registration {
    name: String,
    api_version: u32,
    world: Option<String>,
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
        bytes: Uint8Array,
        contract: Uint8Array,
        configuration: String,
    ) -> napi::Result<NodeComponentPort> {
        let config: Registration = serde_json::from_str(&configuration).map_err(js_error)?;
        let contract =
            Contract::from_bytes(&contract, config.world.as_deref()).map_err(js_error)?;
        let port = self
            .engine
            .open_component(
                &contract,
                &config.name,
                config.api_version,
                &bytes,
                &config.bindings,
                &config.grants,
            )
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
    /// Invoke through the real Canonical ABI, leaving Node free to service other managed calls.
    #[napi]
    pub fn call(
        &self,
        interface: String,
        method: String,
        arguments: String,
    ) -> napi::Result<AsyncTask<ComponentCall>> {
        Ok(AsyncTask::new(ComponentCall {
            port: self
                .port
                .as_ref()
                .ok_or_else(|| js_error("component is closed"))?
                .clone(),
            interface,
            method,
            arguments: serde_json::from_str(&arguments).map_err(js_error)?,
        }))
    }
    /// Release only the facade's claim; any actual task retains its own endpoint until completion.
    #[napi]
    pub fn close(&mut self) {
        self.port.take();
    }
}

pub struct ComponentCall {
    port: Arc<ComponentPort>,
    interface: String,
    method: String,
    arguments: Value,
}
impl Task for ComponentCall {
    type Output = String;
    type JsValue = String;
    /// Wait on the component worker rather than moving a local Tembra kernel across threads.
    fn compute(&mut self) -> napi::Result<String> {
        Ok(envelope(futures::executor::block_on(self.port.call(
            &self.interface,
            &self.method,
            self.arguments.clone(),
        )))
        .to_string())
    }
    /// Hand only data back to the original JavaScript callback frame.
    fn resolve(&mut self, _env: Env, value: String) -> napi::Result<String> {
        Ok(value)
    }
}
