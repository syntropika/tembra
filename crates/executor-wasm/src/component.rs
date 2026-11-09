//! Compiled-language components use the same Orb registration and kernel-owned call lifecycle.
mod values;
mod worker;
use crate::{Binding, Engine, Grants, failure};
use futures::channel::oneshot;
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc};
use tembra::{Orb, Result, Service};

/// A compiled component endpoint contains no thread-affine kernel callbacks or guest handles.
pub struct ComponentPort {
    session: worker::Session,
    admission: futures::lock::Mutex<()>,
    contract: tembra_contracts::PreparedContract,
    /// Export slots bound once at open, keyed by interface identity and original WIT method name.
    slots: BTreeMap<String, BTreeMap<String, u32>>,
    description: Value,
    metadata: tembra::core::Orb,
    _component: Arc<wasmtime::component::Component>,
}
impl ComponentPort {
    /// Read the WIT-derived plan after executable signatures and grants have been checked.
    pub fn description(&self) -> &Value {
        &self.description
    }

    /// Execute one bounded Canonical ABI call while the embedding host retains its invocation.
    pub async fn call(&self, interface: &str, method: &str, arguments: Value) -> Result<Value> {
        let slot = self.slot(interface, method)?;
        self.call_slot(slot, interface, method, arguments, None)
            .await
    }

    /// Queue one call without a waiting thread; the component worker publishes the checked result.
    ///
    /// `complete` runs exactly once. It runs on the caller when the call is refused before
    /// admission, and on the component worker otherwise. A completion that releases the last
    /// owner of this port on the worker lets that worker finish by itself.
    pub fn submit(
        &self,
        interface: &str,
        method: &str,
        arguments: Value,
        complete: impl FnOnce(Result<Value>) + Send + 'static,
    ) {
        match self.slot(interface, method) {
            Ok(slot) => self.submit_slot(slot, interface, method, arguments, complete),
            Err(error) => complete(Err(error)),
        }
    }

    /// Find the slot bound at open without scanning the contract.
    fn slot(&self, interface: &str, method: &str) -> Result<u32> {
        self.slots
            .get(interface)
            .and_then(|methods| methods.get(method))
            .copied()
            .ok_or_else(|| failure(format!("unknown WIT export {interface}#{method}")))
    }

    /// Await one submitted call through a slot bound to this port's contract.
    async fn call_slot(
        &self,
        slot: u32,
        interface: &str,
        method: &str,
        arguments: Value,
        cancellation: Option<tembra::Cancellation>,
    ) -> Result<Value> {
        // Rust callbacks wait outside the worker queue, where their thread-affine cancellation
        // can still withdraw them. Node uses submit() and its own host admission without libuv.
        let admission = std::pin::pin!(self.admission.lock());
        let _permit = if let Some(cancellation) = cancellation.as_ref() {
            let cancelled = std::pin::pin!(cancellation.cancelled());
            match futures::future::select(admission, cancelled).await {
                futures::future::Either::Left((permit, _)) => {
                    cancellation.check()?;
                    permit
                }
                futures::future::Either::Right(_) => {
                    cancellation.check()?;
                    return Err(failure("cancellation completed without a request"));
                }
            }
        } else {
            admission.await
        };
        let (reply, receive) = oneshot::channel();
        self.submit_slot(slot, interface, method, arguments, move |result| {
            let _ = reply.send(result);
        });
        receive.await.map_err(failure)?
    }

    /// Validate both sides of one call; the result is checked before its completion observes it.
    fn submit_slot(
        &self,
        slot: u32,
        interface: &str,
        method: &str,
        arguments: Value,
        complete: impl FnOnce(Result<Value>) + Send + 'static,
    ) {
        if let Err(error) = self.contract.validate_slot(slot, true, &arguments) {
            complete(Err(failure(error)));
            return;
        }
        let contract = self.contract.clone();
        self.session.submit(
            interface.into(),
            method.into(),
            arguments,
            Box::new(move |result: Result<Value>| {
                complete(result.and_then(|value| {
                    contract
                        .validate_slot(slot, false, &value)
                        .map_err(failure)?;
                    Ok(value)
                }));
            }),
        );
    }
}

/// Bind every function of every identified export interface, the set a caller may address.
fn export_slots(
    contract: &tembra_contracts::PreparedContract,
) -> Result<BTreeMap<String, BTreeMap<String, u32>>> {
    let mut slots = BTreeMap::<String, BTreeMap<String, u32>>::new();
    for entry in &contract.model().exports {
        let tembra_contracts::Item::Interface(interface) = &entry.item else {
            continue;
        };
        // Inline interfaces have no identity, so no caller can address them.
        let Some(identity) = &interface.contract else {
            continue;
        };
        let methods = slots.entry(identity.clone()).or_default();
        for function in &interface.functions {
            let slot = contract
                .bind(identity, &function.name, true)
                .map_err(failure)?;
            methods.insert(function.name.clone(), slot);
        }
    }
    Ok(slots)
}

impl Engine {
    /// Materialize a compiled endpoint for an existing platform-owned lifecycle adapter.
    #[doc(hidden)]
    pub fn open_component(
        &self,
        artifact: &tembra_contracts::Artifact,
        bindings: &BTreeMap<String, Binding>,
        grants: &Grants,
    ) -> Result<ComponentPort> {
        let contract = &artifact.contract;
        let name = &artifact.identity.name;
        let api_version = artifact.identity.api_version;
        let tembra_contracts::ArtifactImplementation::Component(bytes) = &artifact.implementation
        else {
            return Err(failure("compiled endpoint requires a component artifact"));
        };
        let plan = contract
            .plan(name, api_version, bindings)
            .map_err(failure)?;
        if !plan.orb.requires.is_empty() {
            return Err(failure(
                "compiled component Orb imports require an additional linker adapter",
            ));
        }
        contract.check_value_profile().map_err(failure)?;
        let slots = export_slots(contract)?;
        let wasi = self.providers.context(&plan.host_imports, grants)?;
        let component = self.compiled(bytes)?;
        Ok(ComponentPort {
            session: worker::Session::start(self.clone(), component.as_ref().clone(), wasi)?,
            admission: futures::lock::Mutex::new(()),
            contract: contract.clone(),
            slots,
            description: plan.describe(),
            metadata: plan.orb,
            _component: component,
        })
    }

    /// Register plain WIT interface exports from a compiled component under the same Orb model.
    pub(crate) fn component_orb(
        &self,
        artifact: &tembra_contracts::Artifact,
        bindings: &BTreeMap<String, Binding>,
        grants: &Grants,
    ) -> Result<Orb> {
        let name = &artifact.identity.name;
        let api_version = artifact.identity.api_version;
        let port = Arc::new(self.open_component(artifact, bindings, grants)?);
        let mut orb = Orb::new(name, api_version).isolated();
        for metadata in &port.metadata.services {
            let interface = metadata
                .name
                .strip_prefix(&format!("{name}::"))
                .ok_or_else(|| failure("invalid prepared component service"))?
                .to_owned();
            let create = port.clone();
            let mut service = Service::new(&metadata.name, move |_| Ok(create.clone()))
                .signature(metadata.signature.clone());
            for method in &metadata.methods {
                // Resolve the slot during registration; calls never look up names in the contract.
                let slot = port.slot(&interface, method)?;
                let interface = interface.clone();
                let called = method.clone();
                service =
                    service.method::<Value, Value, _, _>(method, move |port, context, args| {
                        let interface = interface.clone();
                        let method = called.clone();
                        async move {
                            context.check()?;
                            let result = port
                                .call_slot(slot, &interface, &method, args, Some(context.signal()))
                                .await?;
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
