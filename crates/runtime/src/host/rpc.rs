//! Portable RPC entrypoints reuse native managed invocation and physical settlement.
use super::*;

impl Host {
    pub(crate) fn rpc_method_surface(&self, service: &str, principal: &str) -> Vec<String> {
        for orb in self.kernel().installed_orbs() {
            if let Some(declaration) = orb.declaration.services.iter().find(|s| s.name == service) {
                let allowed = self.access(orb.id, service, principal);
                return declaration
                    .methods
                    .iter()
                    .filter(|method| allowed.as_ref().is_none_or(|set| set.contains(*method)))
                    .cloned()
                    .collect();
            }
        }
        Vec::new()
    }

    /// Resolve checked WIT and the current provider revision without entering provider code.
    pub(crate) fn rpc_contract(
        &self,
        reference: &Reference,
    ) -> Result<Option<crate::rpc::resources::Profile>> {
        let address = self.kernel().object_address(reference.object())?;
        for orb in self.kernel().installed_orbs() {
            let Some(service) = orb
                .declaration
                .services
                .iter()
                .find(|service| service.name == address.service)
            else {
                continue;
            };
            let source = self
                .0
                .orbs
                .borrow()
                .get(&orb.id)
                .and_then(|services| services.get(&address.service))
                .and_then(|implementation| implementation.contract.clone());
            let Some((binding, bytes)) = source else {
                return Ok(None);
            };
            let contract = tembra_contracts::PreparedContract::decode(&bytes)
                .map_err(|error| Error::invalid("contract", error.to_string()))?;
            let signature = contract
                .service_signatures()
                .map_err(|error| Error::invalid("contract", error.to_string()))?
                .remove(&binding)
                .ok_or_else(|| Error::invalid("contract", "missing service signature"))?;
            if service.signature.as_ref() != Some(&signature) {
                return Err(Error::invalid(
                    "contract",
                    "service signature differs from its WIT",
                ));
            }
            return Ok(Some(crate::rpc::resources::Profile {
                contract,
                binding,
                signature,
                revision: orb.id,
            }));
        }
        Err(Error::new(
            core::Code::Gone,
            "service",
            "service is no longer installed",
        ))
    }

    pub(crate) fn rpc_spawn(
        &self,
        operation: impl Future<Output = Result<()>> + 'static,
    ) -> Result<()> {
        let physical = self.spawn(operation)?;
        drop(physical);
        Ok(())
    }
}

impl Reference {
    /// A payload-only entry explicitly requires portable capability delivery over RPC.
    pub(crate) fn rpc_payload_method(&self, method: &str) -> Result<bool> {
        let address = self.host.kernel().object_address(self.object)?;
        for orb in self.host.kernel().installed_orbs() {
            let implementations = self.host.0.orbs.borrow();
            if let Some(implementation) = implementations
                .get(&orb.id)
                .and_then(|services| services.get(&address.service))
            {
                return Ok(implementation.portable.contains_key(method)
                    && !implementation.local.contains_key(method));
            }
        }
        Ok(false)
    }

    pub(crate) fn rpc_methods(&self) -> Result<Vec<String>> {
        Ok(self
            .host
            .kernel()
            .reference_methods(self.id)?
            .into_iter()
            .collect())
    }

    pub(crate) async fn rpc_call(
        &self,
        context: &Context,
        method: &str,
        arguments: Value,
    ) -> Result<Value> {
        let arguments =
            if self.foreign_ready.is_none() && arguments.as_array().is_some_and(Vec::is_empty) {
                Value::Null
            } else {
                arguments
            };
        let (issued, receive) = self
            .issue_portable::<_, Value>(context, method, arguments)
            .await?;
        let result = receive.await;
        issued.settled().await;
        result
    }

    pub(crate) fn rpc_state(&self) -> Result<Option<crate::Readable<Value>>> {
        self.status()?;
        match self.host.kernel().snapshot(self.object) {
            Ok(_) => Ok(Some(crate::Readable::portable(
                self.host.clone(),
                self.object,
                self.released.clone(),
            ))),
            Err(error) if error.code == core::Code::Gone && error.reason == "state" => Ok(None),
            Err(error) => Err(error),
        }
    }
}
