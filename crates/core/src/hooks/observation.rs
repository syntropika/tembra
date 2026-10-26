//! Exact source anchors and temporary physical-use claims for one hook observation.

use crate::{
    Code, Error, Host, Id, Invocation, Owner, Reason, Result,
    host::{Inner, conflict, gone},
    index::Index,
    program::WaitKey,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
};

/// One admitted observation and the immutable plan selected by its source version.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HookObservation {
    pub invocation: Invocation,
    pub plan: Rc<[Id]>,
}

#[derive(Default)]
pub(crate) struct Observations {
    roots: BTreeMap<Id, Observation>,
    scopes: Index<Id>,
    calls: BTreeMap<Id, (Id, BTreeSet<Id>)>,
    consumed: BTreeSet<Id>,
}

struct Observation {
    anchors: BTreeSet<Id>,
    uses: BTreeMap<Id, usize>,
}

impl Host {
    /// Admit one declared hook observation from its physically retained source invocation.
    ///
    /// The source may be cancelled or logically ended. Success consumes its single observation
    /// admission across all hooks, including an empty plan or later participant failure. An
    /// observation invocation or any of its descendants cannot start another observation.
    ///
    /// The returned ordinary invocation retains the source's principal, exact version, orb and
    /// actual object, even for an orb-bound handle (`object == None`). Its frame is empty. Source
    /// owner scopes and their registered ancestors remain anchored until the observation root
    /// ends and every physical descendant settles. Other scopes are claimed only during their
    /// active calls; closing an already completed dependency does not cancel the observation.
    /// Closing an anchored or actively used scope cancels the root and waits for its claims.
    ///
    /// The adapter must finish the invocation and end its root after physical settlement.
    /// Cancellation is cooperative; this method does not reverse external effects.
    ///
    /// # Errors
    /// Returns `gone/call` for an expired source, `forbidden/observation_source` for observation
    /// ancestry, `conflict/observation_admitted` for an already consumed source, and
    /// `forbidden/operation_owner` for a different orb or object-bound owner. Undeclared hooks,
    /// missing/closing scopes, host shutdown and ordinary version gates also refuse admission.
    /// Refusal consumes no observation admission and retains no new root, call or scope claims.
    pub fn begin_hook_observation(
        &self,
        source_call: Id,
        orb: Id,
        operation: Id,
        object: Option<Id>,
        hook: &str,
    ) -> Result<HookObservation> {
        let (source, root, target, anchors) = {
            let mut inner = self.inner.borrow_mut();
            inner.open()?;
            let source = inner
                .calls
                .get(&source_call)
                .cloned()
                .ok_or_else(|| gone(Reason::Call))?;
            if inner.observations.roots.contains_key(&source.root) {
                return Err(Error::new(
                    Code::Forbidden,
                    Reason::ObservationSource,
                    "observation cannot be reopened",
                ));
            }
            if inner.observations.consumed.contains(&source_call) {
                return Err(conflict(Reason::ObservationAdmitted));
            }
            if source.orb != orb
                || object.is_some_and(|object| object != source.object || object == 0)
            {
                return Err(Error::new(
                    Code::Forbidden,
                    Reason::OperationOwner,
                    "hook belongs to another physical owner",
                ));
            }
            let declaration = &inner.orbs[&orb].declaration;
            if !declaration.hooks.contains(hook) {
                return Err(gone(Reason::Hook));
            }
            if operation == 0 {
                return Err(Error::invalid(
                    Reason::Operation,
                    "operation identity is required",
                ));
            }
            let target = declaration.name.clone();
            let anchors = inner.observation_scopes(&source)?;
            let principal = inner.roots[&source.root].principal.clone();
            let root = inner.root(source.code_version, principal, false)?;
            (source, root, target, anchors)
        };
        let result = (|| {
            let plan = self.hook_plan(root, &target, hook)?;
            {
                let mut inner = self.inner.borrow_mut();
                for scope in &anchors {
                    inner.observations.scopes.insert(*scope, root);
                }
                inner.observations.roots.insert(
                    root,
                    Observation {
                        anchors,
                        uses: BTreeMap::new(),
                    },
                );
            }
            let invocation = self.begin_operation(
                orb,
                root,
                operation,
                (source.object != 0).then_some(source.object),
            )?;
            self.inner
                .borrow_mut()
                .observations
                .consumed
                .insert(source_call);
            Ok(HookObservation { invocation, plan })
        })();
        if result.is_err() {
            self.end_root(root)?;
        }
        result
    }
}

impl Inner {
    /// Resolve exact installed owners and their explicit scope ancestry, without root history.
    fn observation_scopes(&self, call: &Invocation) -> Result<BTreeSet<Id>> {
        let orb = self.orbs.get(&call.orb).ok_or_else(|| gone(Reason::Orb))?;
        let mut scopes = BTreeSet::new();
        for owner in [
            Some(Owner::Orb(orb.owner)),
            (call.object != 0).then_some(Owner::Object(call.object)),
        ]
        .into_iter()
        .flatten()
        {
            let owned: Vec<_> = self
                .owner_scopes
                .members(&owner)
                .filter(|id| self.scopes[id].orb == call.orb)
                .collect();
            if owned.is_empty() {
                return Err(gone(Reason::Scope));
            }
            scopes.extend(owned);
        }
        let mut pending: Vec<_> = scopes.iter().copied().collect();
        while let Some(id) = pending.pop() {
            let scope = self.scopes.get(&id).ok_or_else(|| gone(Reason::Scope))?;
            if scope.closing {
                return Err(conflict(Reason::ScopeClosed));
            }
            for parent in &scope.parents {
                if scopes.insert(*parent) {
                    pending.push(*parent);
                }
            }
        }
        Ok(scopes)
    }

    /// Validate before retaining a call; ordinary roots never inspect scope ownership here.
    pub(crate) fn claim_observation_call(&mut self, call: &Invocation) -> Result<()> {
        if self.observations.roots.is_empty() || !self.observations.roots.contains_key(&call.root) {
            return Ok(());
        }
        let scopes = self.observation_scopes(call)?;
        let record = self
            .observations
            .roots
            .get_mut(&call.root)
            .expect("observation root");
        for scope in &scopes {
            *record.uses.entry(*scope).or_default() += 1;
            self.observations.scopes.insert(*scope, call.root);
        }
        self.observations.calls.insert(call.id, (call.root, scopes));
        Ok(())
    }

    /// Release incidental scope use at actual call settlement, independently from root history.
    pub(crate) fn release_observation_call(&mut self, call: Id) {
        self.observations.consumed.remove(&call);
        let Some((root, scopes)) = self.observations.calls.remove(&call) else {
            return;
        };
        let record = self
            .observations
            .roots
            .get_mut(&root)
            .expect("physical call retains observation");
        for scope in &scopes {
            let count = record.uses.get_mut(scope).expect("claimed scope");
            *count -= 1;
            if *count == 0 {
                record.uses.remove(scope);
                if !record.anchors.contains(scope) {
                    self.observations.scopes.remove(scope, root);
                }
            }
        }
        for scope in scopes {
            self.wake(WaitKey::Scope(scope));
        }
    }

    /// Release only this observation's exact source anchors after its final physical descendant.
    pub(crate) fn remove_observation_root(&mut self, root: Id) {
        let Some(record) = self.observations.roots.remove(&root) else {
            return;
        };
        debug_assert!(record.uses.is_empty());
        for scope in record.anchors {
            self.observations.scopes.remove(&scope, root);
            self.wake(WaitKey::Scope(scope));
        }
    }

    /// A missing bucket makes ordinary cleanup independent of the ordinary root population.
    pub(crate) fn scope_has_observations(&self, scope: Id) -> bool {
        self.observations.scopes.contains(&scope)
    }

    /// Cancel only roots with an exact anchor or current physical claim on this scope.
    pub(crate) fn cancel_scope_observations(&mut self, scope: Id) {
        let roots: Vec<_> = self.observations.scopes.members(&scope).collect();
        for root in roots {
            let record = self.roots.get_mut(&root).expect("scope claim retains root");
            if !record.cancelled {
                record.cancelled = true;
                self.actions.push_back(crate::Action::CancelRoot(root));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Orb, Scope};

    #[test]
    fn refused_and_settled_admissions_release_private_claims_and_consumption_records() {
        let host = Host::new();
        let mut declaration = Orb::new("provider", 1);
        declaration.hooks.insert("terminal".into());
        let reload = host.prepare(vec![declaration.clone()], vec![]).unwrap();
        let orb = reload.orbs[0].clone();
        host.commit(reload.id, vec![]).unwrap();
        let scope = host.allocate_handle().unwrap();
        host.register_scope(Scope {
            id: scope,
            owner: Owner::Orb(orb.owner),
            orb: orb.id,
            parents: vec![],
            dependencies: vec![],
            resources: vec![],
            state_version: None,
            migrate: false,
            active: true,
            closing: false,
        })
        .unwrap();
        let root = host.begin_root("alice").unwrap();
        let source = host
            .begin_operation(orb.id, root, host.allocate_handle().unwrap(), None)
            .unwrap();
        let replacement = host.prepare(vec![declaration], vec![]).unwrap();
        assert!(!host.gate_orbs(replacement.id, &[orb.owner]).unwrap());
        let operation = host.allocate_handle().unwrap();
        assert!(
            host.begin_hook_observation(source.id, orb.id, operation, None, "terminal")
                .is_err()
        );
        {
            let inner = host.inner.borrow();
            assert!(inner.observations.roots.is_empty());
            assert!(inner.observations.calls.is_empty());
            assert!(inner.observations.consumed.is_empty());
            assert!(!inner.observations.scopes.contains(&scope));
        }
        host.abort(replacement.id).unwrap();
        let observation = host
            .begin_hook_observation(source.id, orb.id, operation, None, "terminal")
            .unwrap()
            .invocation;
        host.finish_call(observation.id).unwrap();
        host.end_root(observation.root).unwrap();
        {
            let inner = host.inner.borrow();
            assert!(inner.observations.roots.is_empty());
            assert!(inner.observations.calls.is_empty());
            assert!(inner.observations.consumed.contains(&source.id));
            assert!(!inner.observations.scopes.contains(&scope));
        }
        host.end_root(root).unwrap();
        host.finish_call(source.id).unwrap();
        let inner = host.inner.borrow();
        assert!(inner.observations.consumed.is_empty());
        assert!(inner.roots.is_empty());
        assert!(inner.calls.is_empty());
    }
}
