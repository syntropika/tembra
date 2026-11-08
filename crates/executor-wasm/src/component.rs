//! Compiled-language components use the same Orb registration and kernel-owned call lifecycle.
mod values;
mod worker;
use crate::{Binding, Contract, Engine, Grants, failure};
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc};
use tembra::{Orb, Result, Service};

/// A compiled component endpoint contains no thread-affine kernel callbacks or guest handles.
pub struct ComponentPort {
    session: worker::Session,
    contract: Contract,
    description: Value,
}
impl ComponentPort {
    /// Read the WIT-derived plan after executable signatures and grants have been checked.
    pub fn description(&self) -> &Value {
        &self.description
    }

    /// Execute one bounded Canonical ABI call while the embedding host retains its invocation.
    pub async fn call(&self, interface: &str, method: &str, arguments: Value) -> Result<Value> {
        self.contract
            .validate_arguments(interface, method, &arguments)
            .map_err(failure)?;
        let value = self
            .session
            .call(interface.into(), method.into(), arguments)
            .await?;
        self.contract
            .validate_result(interface, method, &value)
            .map_err(failure)?;
        Ok(value)
    }
}

impl Engine {
    /// Materialize a compiled endpoint for an existing platform-owned lifecycle adapter.
    pub fn open_component(
        &self,
        contract: &Contract,
        name: &str,
        api_version: u32,
        bytes: &[u8],
        bindings: &BTreeMap<String, Binding>,
        grants: &Grants,
    ) -> Result<ComponentPort> {
        let plan = contract
            .plan(name, api_version, bindings)
            .map_err(failure)?;
        if !plan.orb.requires.is_empty() {
            return Err(failure(
                "compiled component Orb imports require an additional linker adapter",
            ));
        }
        contract.check_value_profile().map_err(failure)?;
        contract.check_component_exports(bytes).map_err(failure)?;
        let wasi = self.providers.context(&plan.host_imports, grants)?;
        let component =
            wasmtime::component::Component::new(&self.engine, bytes).map_err(failure)?;
        Ok(ComponentPort {
            session: worker::Session::start(self.clone(), component, wasi)?,
            contract: contract.clone(),
            description: plan.describe(),
        })
    }

    /// Register plain WIT interface exports from a compiled component under the same Orb model.
    pub fn prepare_component(
        &self,
        contract: &Contract,
        name: &str,
        api_version: u32,
        bytes: &[u8],
        bindings: &BTreeMap<String, Binding>,
        grants: &Grants,
    ) -> Result<Orb> {
        let plan = contract
            .plan(name, api_version, bindings)
            .map_err(failure)?;
        let port =
            Arc::new(self.open_component(contract, name, api_version, bytes, bindings, grants)?);
        let mut orb = Orb::new(name, api_version);
        for metadata in plan.orb.services {
            let interface = metadata
                .name
                .strip_prefix(&format!("{name}::"))
                .ok_or_else(|| failure("invalid prepared component service"))?
                .to_owned();
            let create = port.clone();
            let mut service = Service::new(metadata.name, move |_| Ok(create.clone()));
            for method in metadata.methods {
                let interface = interface.clone();
                let called = method.clone();
                service =
                    service.method::<Value, Value, _, _>(method, move |port, context, args| {
                        let interface = interface.clone();
                        let method = called.clone();
                        async move {
                            context.check()?;
                            let result = port.call(&interface, &method, args).await?;
                            context.check()?;
                            Ok(result)
                        }
                    });
            }
            orb = orb.service(service);
        }
        Ok(orb)
    }
}
