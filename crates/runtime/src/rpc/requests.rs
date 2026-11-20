//! Reserve connection claims in frame order, then use ordinary managed native admission.
use super::*;
use crate::portable::Payload;
use resources::Profile;

/// A received frame owns its receiver reservation and every irreversible argument move.
pub(super) struct PreparedCall {
    target: Target,
    reference: Rc<Reference>,
    profile: Option<Profile>,
    method: String,
    arguments: Value,
    payload: Option<Payload>,
}

impl Inner {
    /// Preflight the selected channel and take moves before queueing or refusing work.
    pub(super) fn prepare_call(&self, payload: Value) -> Result<PreparedCall> {
        #[derive(Deserialize)]
        struct Call {
            reference: u64,
            method: String,
            arguments: Option<Vec<Value>>,
            input: Option<String>,
            capabilities: Option<Capabilities>,
        }
        let input: Call = serde_json::from_value(payload).map_err(invalid)?;
        let target = {
            let claims = self.claims.borrow();
            let claim = claims
                .get(&input.reference)
                .ok_or_else(|| Error::new(Code::Forbidden, "method", "unknown reference"))?;
            if !claim.methods.contains(&input.method) {
                return Err(Error::new(
                    Code::Forbidden,
                    "method",
                    "method is not admitted",
                ));
            }
            if input.capabilities.is_some() && !claim.resources {
                return Err(resources::unsupported());
            }
            claim.target.clone()
        };
        if matches!(target, Target::Resource(..)) && input.capabilities.is_none() {
            return Err(resources::unsupported());
        }
        let reference = target.reference()?;
        let profile = self.host.rpc_contract(&reference)?;
        if let Target::Resource(owner, _) = &target
            && profile
                .as_ref()
                .is_none_or(|profile| profile.revision != owner.revision())
        {
            return Err(Error::new(
                Code::Gone,
                "resource",
                "issuing provider was reloaded",
            ));
        }
        let needs_resources = if let Some(profile) = &profile {
            let slot = profile
                .contract
                .bind(&profile.binding, &input.method, true)
                .map_err(|error| Error::invalid("contract", error.to_string()))?;
            let roles = profile
                .contract
                .resource_signature(slot)
                .map_err(|error| Error::invalid("contract", error.to_string()))?;
            if roles.stream_result {
                return Err(resources::unsupported());
            }
            roles.receiver.is_some() || roles.owned_arguments || roles.owned_results
        } else {
            reference.rpc_payload_method(&input.method)?
        };
        if needs_resources
            && (input.capabilities.is_none() || !reference.rpc_payload_method(&input.method)?)
        {
            return Err(resources::unsupported());
        }
        if input.capabilities.is_some() && profile.is_none() {
            return Err(resources::unsupported());
        }
        let arguments = match (&input.capabilities, input.arguments, input.input) {
            (None, Some(arguments), None) => Value::Array(arguments),
            (Some(_), None, Some(input)) => serde_json::from_str(&input).map_err(invalid)?,
            _ => {
                return Err(invalid(
                    "a call sends either ordinary arguments or captured WIT input",
                ));
            }
        };
        let payload = match input.capabilities {
            Some(capabilities) => Some(self.prepare_resources(
                &target,
                profile.as_ref().expect("checked profile"),
                &input.method,
                arguments.clone(),
                capabilities,
            )?),
            None => None,
        };
        Ok(PreparedCall {
            target,
            reference,
            profile,
            method: input.method,
            arguments,
            payload,
        })
    }

    /// Execute a frame's reserved claims through the host's existing root and managed calls.
    pub(super) async fn call(self: &Rc<Self>, id: u64, prepared: PreparedCall) -> Result<Value> {
        let state = self.clone();
        self.host
            .root(&self.options.principal, move |ctx| {
                async move {
                    if state.closed.get() || state.requests.borrow().get(&id) == Some(&true) {
                        return Err(Error::new(Code::Cancelled, "request", "request cancelled"));
                    }
                    state.calls.borrow_mut().insert(id, ctx.clone());
                    let result = match prepared.payload {
                        Some(payload) => {
                            state
                                .call_resources(
                                    id,
                                    prepared.target,
                                    prepared.profile.expect("checked profile"),
                                    &prepared.method,
                                    payload,
                                    &ctx,
                                )
                                .await
                        }
                        None => {
                            prepared
                                .reference
                                .rpc_call(&ctx, &prepared.method, prepared.arguments)
                                .await
                        }
                    };
                    state.calls.borrow_mut().remove(&id);
                    result
                }
                .boxed_local()
            })
            .await
    }
}
