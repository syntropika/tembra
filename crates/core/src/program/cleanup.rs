//! Release only the scopes and resources owned by this program.

use super::*;

impl Host {
    /// Create a cleanup program for the supplied owned scopes.
    pub fn cleanup_scopes(&self, scopes: Vec<Id>, object: Option<Id>) -> Result<Id> {
        let mut inner = self.inner.borrow_mut();
        if scopes.iter().any(|id| !inner.scopes.contains_key(id)) {
            return Err(gone(Reason::Scope));
        }
        inner.begin_cleanup(&scopes);
        let id = inner.id()?;
        let mut program = Program::new(scopes, Phase::Stop);
        program.object = object;
        inner.programs.insert(id, program);
        Ok(id)
    }

    /// Remove a settled scope and release its dependency claims.
    pub(super) fn drop_scope(&self, scope: Id) -> Result<bool> {
        let mut inner = self.inner.borrow_mut();
        let Some(record) = inner.scopes.remove(&scope) else {
            return Ok(false);
        };
        if !record.closing {
            inner.track_scope_version(&record, false);
        }
        inner.scope_code_versions.remove(&scope);
        if record.owner != Owner::Host {
            inner.non_host_scopes -= 1;
            if inner.non_host_scopes == 0 {
                inner.wake(WaitKey::HostScopes);
            }
        }
        inner.owner_scopes.remove(&record.owner, scope);
        inner.orb_scopes.remove(&record.orb, scope);
        for parent in &record.parents {
            inner.scope_children.remove(parent, scope);
            inner.wake(WaitKey::Scope(*parent));
        }
        for resource in &record.resources {
            inner.resource_scopes.remove(resource, scope);
            inner.resource_claims.remove(resource, scope);
            inner.wake(WaitKey::Resource(*resource));
        }
        inner.wake(WaitKey::Scope(scope));
        for reference in &record.dependencies {
            inner.reference_scopes.remove(reference, scope);
            if let Some(target) = inner.references.get(reference).map(|claim| claim.object) {
                inner.scope_dependencies.remove(&target, scope);
            }
        }
        drop(inner);
        for reference in record.dependencies {
            if let Owner::Object(object) = record.owner {
                self.detach_dependency(object, reference)?;
            }
            if self.inner.borrow().references.contains_key(&reference) {
                self.release(reference)?;
                self.forget_reference(reference)?;
            }
        }
        Ok(true)
    }

    /// Children settle before parent scopes; resource dependencies determine order within a scope.
    pub(super) fn cleanup_batch(
        &self,
        program_id: Id,
        program: &mut Program,
    ) -> Result<Option<Vec<Effect>>> {
        let leaves: Vec<_> = {
            let inner = self.inner.borrow();
            program
                .scopes
                .iter()
                .filter(|id| {
                    let leased = inner
                        .scope_programs
                        .members(id)
                        .filter(|active| *active != program_id)
                        .collect::<Vec<_>>();
                    if !leased.is_empty() {
                        program
                            .waiting
                            .extend(leased.into_iter().map(WaitKey::Program));
                        return false;
                    }
                    if inner.closing
                        && inner.non_host_scopes != 0
                        && inner
                            .scopes
                            .get(id)
                            .is_some_and(|scope| scope.owner == Owner::Host)
                    {
                        program.waiting.insert(WaitKey::HostScopes);
                        return false;
                    }
                    if inner.scope_children.contains(id)
                        || inner
                            .scope_activations
                            .get(id)
                            .is_some_and(|active| *active != program_id)
                    {
                        program.waiting.insert(WaitKey::Scope(**id));
                        false
                    } else {
                        true
                    }
                })
                .copied()
                .collect()
        };
        if leaves.is_empty() {
            return Ok(program.scopes.is_empty().then(Vec::new));
        }
        let mut effects = Vec::new();
        let mut dropped = BTreeSet::new();
        for scope in leaves {
            #[cfg(feature = "hooks")]
            if self.inner.borrow().scope_has_observations(scope) {
                program.waiting.insert(WaitKey::Scope(scope));
                continue;
            }
            let mut ids = self.scope_resources(&[scope]);
            ids.retain(|id| !self.inner.borrow().resource_claims.contains(id));
            let Some(ready) = self.closers(&ids, &program.retained, None, &mut program.waiting)?
            else {
                continue;
            };
            if ready.is_empty() {
                if self.drop_scope(scope)? {
                    effects.push(Effect::Drop { scope });
                }
                dropped.insert(scope);
            } else {
                effects.extend(ready.into_iter().map(|resource| Effect::Close { resource }));
            }
        }
        program.scopes.retain(|id| !dropped.contains(id));
        Ok((!effects.is_empty() || program.scopes.is_empty()).then_some(effects))
    }

    /// Collect resources owned by the supplied scopes only.
    pub(super) fn scope_resources(&self, scopes: &[Id]) -> BTreeSet<Id> {
        let inner = self.inner.borrow();
        scopes
            .iter()
            .filter_map(|id| inner.scopes.get(id))
            .flat_map(|scope| scope.resources.iter().copied())
            .collect()
    }

    /// Keep cleanup blocked while replacement effects hold scope leases.
    // Scope leases prevent cleanup from racing a reload's pending adapter effects.
    pub(super) fn wait_for_reloads(&self, id: Id, program: &mut Program) -> bool {
        let inner = self.inner.borrow();
        for scope in &program.scopes {
            program.waiting.extend(
                inner
                    .scope_programs
                    .members(scope)
                    .filter(|active| *active != id)
                    .map(WaitKey::Program),
            );
        }
        !program.waiting.is_empty()
    }

    /// Admit resource closures in dependency and owner-settlement order.
    pub(super) fn closers(
        &self,
        ids: &BTreeSet<Id>,
        retained: &BTreeSet<Id>,
        before: Option<Version>,
        waiting: &mut BTreeSet<WaitKey>,
    ) -> Result<Option<Vec<Id>>> {
        let (remaining, ready) = {
            let inner = self.inner.borrow();
            let remaining: Vec<_> = ids
                .iter()
                .filter(|id| !retained.contains(id) && inner.resources.contains_key(id))
                .copied()
                .collect();
            let ready = remaining
                .iter()
                .filter(|id| !inner.dependents.contains(id) && !inner.resources[id].closing)
                .copied()
                .collect::<Vec<_>>();
            for id in &remaining {
                waiting.insert(WaitKey::Resource(*id));
            }
            (remaining, ready)
        };
        if remaining.is_empty() {
            return Ok(Some(Vec::new()));
        }
        if ready.is_empty() {
            return Ok(None);
        }
        let mut admitted = Vec::new();
        for resource in ready {
            let cutoff = before.or_else(|| {
                let inner = self.inner.borrow();
                let owner = inner.resources[&resource].owner;
                inner
                    .scope_versions
                    .versions(&owner)
                    .find(|(version, count)| {
                        *count > inner.resource_versions.count(&resource, *version)
                    })
                    .map(|(version, _)| version)
            });
            if self.close_resource_before(resource, cutoff)? {
                admitted.push(resource);
            } else {
                waiting.insert(WaitKey::Owner(
                    self.inner.borrow().resources[&resource].owner,
                ));
            }
        }
        Ok((!admitted.is_empty()).then_some(admitted))
    }

    /// Move indexed scope and resource dependencies to a restored identity.
    pub(super) fn replace_resource(&self, previous: Id, next: Id) {
        let mut inner = self.inner.borrow_mut();
        for scope in inner.resource_scopes.take(&previous) {
            let record = inner.scopes.get_mut(&scope).expect("indexed scope");
            for resource in &mut record.resources {
                if *resource == previous {
                    *resource = next;
                }
            }
            let claimed = !record.closing;
            inner.resource_scopes.insert(next, scope);
            inner.resource_claims.remove(&previous, scope);
            if claimed {
                inner.resource_claims.insert(next, scope);
                if let Some(version) = inner.scope_code_versions.get(&scope).copied() {
                    inner.resource_versions.remove(&previous, version);
                    inner.resource_versions.insert(next, version);
                }
            }
        }
        for resource in inner.dependents.take(&previous) {
            for dependency in &mut inner
                .resources
                .get_mut(&resource)
                .expect("indexed resource")
                .dependencies
            {
                if *dependency == previous {
                    *dependency = next;
                }
            }
            inner.dependents.insert(next, resource);
        }
        inner.wake(WaitKey::Resource(previous));
    }
}
