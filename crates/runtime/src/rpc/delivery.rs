//! Atomically publish portable owners only after a complete bounded response is ready.
use super::*;
use crate::portable::Delivered;
use resources::{Profile, Resource, Target, unsupported};

impl Inner {
    /// Publish checked owners atomically after the entire response fits the frame budget.
    pub(super) fn deliver(
        &self,
        id: u64,
        profile: &Profile,
        slot: u32,
        delivered: Delivered,
    ) -> Result<Value> {
        let Delivered {
            mut value,
            owned,
            streams,
        } = delivered;
        if !streams.is_empty() {
            return Err(unsupported());
        }
        if self.claims.borrow().len().saturating_add(owned.len()) > self.options.max_references {
            return Err(Error::new(
                Code::Exhausted,
                "remote_references",
                "reference limit",
            ));
        }
        let mut owners: Vec<_> = owned.into_iter().map(Some).collect();
        let mut additions = Vec::new();
        profile
            .contract
            .map_owned_result(slot, &mut value, |nominal, index| {
                let owner = owners
                    .get_mut(index as usize)
                    .and_then(Option::take)
                    .ok_or_else(|| tembra_contracts::Error("invalid owned result index".into()))?;
                if nominal.interface.as_ref() != Some(&owner.nominal().interface)
                    || nominal.name.as_ref() != Some(&owner.nominal().name)
                {
                    return Err(tembra_contracts::Error(
                        "result resource has another nominal identity".into(),
                    ));
                }
                let reference = self
                    .allocate()
                    .map_err(|error| tembra_contracts::Error(error.to_string()))?;
                let mut token = [0_u8; 32];
                getrandom::fill(&mut token)
                    .map_err(|error| tembra_contracts::Error(error.to_string()))?;
                let descriptor = Resource {
                    reference,
                    token: token.iter().map(|byte| format!("{byte:02x}")).collect(),
                    interface: owner.nominal().interface.clone(),
                    name: owner.nominal().name.clone(),
                    revision: owner.revision().to_string(),
                };
                let methods = owner
                    .reference()
                    .and_then(|reference| reference.rpc_methods())
                    .map_err(|error| tembra_contracts::Error(error.to_string()))?
                    .into_iter()
                    .collect();
                additions.push((
                    descriptor.clone(),
                    Claim {
                        target: Target::Resource(Rc::new(owner), descriptor),
                        methods,
                        observation: None,
                        active: true,
                        pending: None,
                        resources: true,
                        delivery: Some(id),
                    },
                ));
                Ok((additions.len() - 1) as u32)
            })
            .map_err(|error| Error::invalid("result", error.to_string()))?;
        if owners.iter().any(Option::is_some) {
            return Err(Error::invalid("result", "unused owned result"));
        }
        let value = profile
            .contract
            .to_resource_wire(slot, false, value)
            .map_err(|error| Error::invalid("result", error.to_string()))?;
        let output = serde_json::to_string(&value).map_err(invalid)?;
        let result = json!({"output": output, "owned": additions.iter().map(|(descriptor, _)| descriptor).collect::<Vec<_>>()});
        self.encode(&json!({"id": id, "ok": result}))?;
        let mut claims = self.claims.borrow_mut();
        for (descriptor, claim) in additions {
            claims.insert(descriptor.reference, claim);
        }
        Ok(result)
    }
}
