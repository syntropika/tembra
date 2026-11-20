//! Checked WIT resources reuse the connection's existing claim table and portable owners.
use super::*;
use crate::portable::{Delivered, Moved, Owned, Payload};
use serde::Serialize;
use tembra_contracts::PreparedContract;

/// The existing contract and issuing revision selected by native admission.
#[derive(Clone)]
pub(crate) struct Profile {
    pub contract: PreparedContract,
    pub binding: String,
    pub signature: String,
    pub revision: crate::core::Id,
}

/// A connection-local claim; its secret never substitutes for ordinary kernel admission.
#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Resource {
    pub reference: u64,
    pub(super) token: String,
    pub(super) interface: String,
    pub(super) name: String,
    pub(super) revision: String,
}

/// The same RPC claim table owns service references and provider-issued resource owners.
#[derive(Clone)]
pub(super) enum Target {
    Service(Rc<Reference>),
    Resource(Rc<Owned>, Resource),
}

impl Target {
    /// Read the exact admission claim retained by this owner.
    pub(super) fn reference(&self) -> Result<Rc<Reference>> {
        match self {
            Self::Service(reference) => Ok(reference.clone()),
            Self::Resource(owner, _) => owner.reference(),
        }
    }
}

/// Optional, negotiated attachment fields on an ordinary RPC call.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Capabilities {
    pub signature: String,
    pub receiver: Option<Resource>,
    #[serde(default)]
    pub owned: Vec<Resource>,
}

/// Refuse an unsupported channel before the selected provider callback runs.
pub(super) fn unsupported() -> Error {
    Error::new(
        Code::Unsupported,
        "capabilities",
        "matching WIT resource delivery is required",
    )
}

/// A malformed or foreign token grants no authority to another connection or resource type.
fn forbidden() -> Error {
    Error::new(
        Code::Forbidden,
        "resource",
        "unknown or invalid resource claim",
    )
}

impl Inner {
    /// Validate every descriptor against the existing table without redeeming an attachment.
    fn owner(&self, supplied: &Resource) -> Result<Rc<Owned>> {
        let owner = match self
            .claims
            .borrow()
            .get(&supplied.reference)
            .map(|claim| &claim.target)
        {
            Some(Target::Resource(owner, actual)) if actual == supplied => owner.clone(),
            _ => return Err(forbidden()),
        };
        let profile = self
            .host
            .rpc_contract(owner.reference()?.as_ref())?
            .ok_or_else(unsupported)?;
        if profile.revision != owner.revision() {
            return Err(Error::new(
                Code::Gone,
                "resource",
                "issuing provider was reloaded",
            ));
        }
        Ok(owner)
    }

    /// Remove a resource only after checking its complete connection-local descriptor.
    pub(super) fn release(&self, payload: Value) -> Result<Value> {
        let reference = payload
            .get("reference")
            .and_then(Value::as_u64)
            .ok_or_else(|| invalid("missing reference"))?;
        let removed = {
            let mut claims = self.claims.borrow_mut();
            if let Some(claim) = claims.get(&reference) {
                match &claim.target {
                    Target::Service(_) if payload.get("resource").is_none() => {}
                    Target::Resource(_, actual) => {
                        let supplied: Resource = serde_json::from_value(
                            payload.get("resource").cloned().ok_or_else(forbidden)?,
                        )
                        .map_err(|_| forbidden())?;
                        if actual != &supplied {
                            return Err(forbidden());
                        }
                    }
                    _ => return Err(forbidden()),
                }
            }
            claims.remove(&reference)
        };
        drop(removed);
        Ok(Value::Null)
    }

    /// Drop unaccepted results on late cancellation, including a response already sent.
    pub(super) fn abandon(&self, id: u64) {
        let removed: Vec<_> = {
            let mut claims = self.claims.borrow_mut();
            let ids: Vec<_> = claims
                .iter()
                .filter_map(|(key, claim)| (claim.delivery == Some(id)).then_some(*key))
                .collect();
            ids.into_iter()
                .filter_map(|key| claims.remove(&key))
                .collect()
        };
        drop(removed);
    }

    /// Accept a received result without allocating or duplicating another claim.
    pub(super) fn accept(&self, id: u64) {
        for claim in self.claims.borrow_mut().values_mut() {
            if claim.delivery == Some(id) {
                claim.delivery = None;
            }
        }
    }

    /// Check shapes, nominal identities and tokens before moving any owned argument.
    fn arguments(
        &self,
        profile: &Profile,
        slot: u32,
        value: Value,
        supplied: &[Resource],
    ) -> Result<Payload> {
        let mut value = profile
            .contract
            .from_resource_wire(slot, true, value)
            .map_err(|error| Error::invalid("arguments", error.to_string()))?;
        let mut used = BTreeSet::new();
        let mut references = BTreeSet::new();
        let mut owners = Vec::new();
        profile
            .contract
            .map_owned_arguments(slot, &mut value, |nominal, index| {
                let descriptor = supplied
                    .get(index as usize)
                    .filter(|_| used.insert(index))
                    .ok_or_else(|| {
                        tembra_contracts::Error("invalid owned argument index".into())
                    })?;
                if !references.insert(descriptor.reference)
                    || nominal.interface.as_ref() != Some(&descriptor.interface)
                    || nominal.name.as_ref() != Some(&descriptor.name)
                {
                    return Err(tembra_contracts::Error(
                        "owned argument has another nominal identity or repeats a claim".into(),
                    ));
                }
                let owner = self
                    .owner(descriptor)
                    .map_err(|error| tembra_contracts::Error(error.to_string()))?;
                if Rc::strong_count(&owner) > 2 {
                    return Err(tembra_contracts::Error(
                        "a pending method still borrows this resource".into(),
                    ));
                }
                owners.push(owner);
                Ok((owners.len() - 1) as u32)
            })
            .map_err(|error| Error::invalid("resource", error.to_string()))?;
        if used.len() != supplied.len() {
            return Err(Error::invalid("resource", "unused owned argument"));
        }
        let mut payload = Payload::data(value);
        for owner in owners {
            let moved = Moved::from_shared(owner)?;
            payload.attach_moved(moved);
        }
        // Moves have revoked the table's aliases before admission can suspend.
        let removed: Vec<_> = {
            let mut claims = self.claims.borrow_mut();
            references
                .into_iter()
                .filter_map(|reference| claims.remove(&reference))
                .collect()
        };
        drop(removed);
        Ok(payload)
    }

    /// Invoke native or compiled providers through one existing portable resource abstraction.
    pub(super) fn prepare_resources(
        &self,
        target: &Target,
        profile: &Profile,
        method: &str,
        arguments: Value,
        capabilities: Capabilities,
    ) -> Result<Payload> {
        if profile.signature != capabilities.signature {
            return Err(unsupported());
        }
        let slot = profile
            .contract
            .bind(&profile.binding, method, true)
            .map_err(|error| Error::invalid("contract", error.to_string()))?;
        let roles = profile
            .contract
            .resource_signature(slot)
            .map_err(|error| Error::invalid("contract", error.to_string()))?;
        if roles.stream_result {
            return Err(unsupported());
        }
        match (target, roles.receiver, &capabilities.receiver) {
            (Target::Service(_), None, None) => {}
            (Target::Resource(owner, actual), Some(nominal), Some(supplied))
                if actual == supplied =>
            {
                let checked = self.owner(supplied)?;
                if !Rc::ptr_eq(owner, &checked)
                    || nominal.interface.as_ref() != Some(&owner.nominal().interface)
                    || nominal.name.as_ref() != Some(&owner.nominal().name)
                {
                    return Err(forbidden());
                }
            }
            _ => return Err(forbidden()),
        }
        if capabilities.receiver.as_ref().is_some_and(|receiver| {
            capabilities
                .owned
                .iter()
                .any(|owner| owner.reference == receiver.reference)
        }) {
            return Err(forbidden());
        }
        self.arguments(profile, slot, arguments, &capabilities.owned)
    }

    /// Native and compiled providers share exactly the same portable call and physical wait.
    pub(super) async fn call_resources(
        &self,
        id: u64,
        target: Target,
        profile: Profile,
        method: &str,
        payload: Payload,
        context: &Context,
    ) -> Result<Value> {
        let reference = target.reference()?;
        let current = self
            .host
            .rpc_contract(&reference)?
            .ok_or_else(unsupported)?;
        if current.signature != profile.signature {
            return Err(unsupported());
        }
        if let Target::Resource(owner, _) = &target
            && current.revision != owner.revision()
        {
            return Err(Error::new(
                Code::Gone,
                "resource",
                "issuing provider was reloaded",
            ));
        }
        let slot = profile
            .contract
            .bind(&profile.binding, method, true)
            .map_err(|error| Error::invalid("contract", error.to_string()))?;
        let (issued, receive) = match &target {
            Target::Service(reference) => {
                reference
                    .issue_portable::<_, Delivered>(context, method, payload)
                    .await?
            }
            Target::Resource(owner, _) => {
                owner
                    .issue_portable::<_, Delivered>(context, method, payload)
                    .await?
            }
        };
        let result = receive.await;
        issued.settled().await;
        let delivered = result?;
        if self.closed.get() || self.requests.borrow().get(&id) == Some(&true) {
            return Err(Error::new(Code::Cancelled, "request", "delivery cancelled"));
        }
        self.deliver(id, &profile, slot, delivered)
    }
}
