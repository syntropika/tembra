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
        if !self.host.same(&context.host) {
            return Err(Error::invalid(Reason::Context, "foreign host context"));
        }
        context.check()?;
        self.ready().await?;
        context.check()?;
        let call = self.host.kernel().begin_call_with_context(
            self.id,
            context.root,
            method,
            context.frame(),
        )?;
        let host = self.host.clone();
        self.host
            .spawn_managed(call.id, async move {
                if host.has_orb(call.orb) {
                    let arguments = if arguments.as_array().is_some_and(Vec::is_empty) {
                        Value::Null
                    } else {
                        arguments
                    };
                    host.invoke_portable(call, arguments).await
                } else {
                    host.foreign(serde_json::json!({"kind":"invoke","call":call.id,"frame":call.context,"input":arguments})).await
                }
            })?
            .await
            .map_err(|_| Error::invalid(Reason::Executor, "portable callback executor stopped"))?
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
