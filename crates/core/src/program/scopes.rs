//! Scope ownership and indexed membership.

use super::*;

impl Host {
    /// Validate ownership and index a declared scope's memberships.
    pub fn register_scope(&self, scope: Scope) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        inner.open()?;
        inner.owner(scope.owner)?;
        if scope.id == 0
            || inner.scopes.contains_key(&scope.id)
            || scope
                .parents
                .iter()
                .any(|id| !inner.scopes.contains_key(id))
        {
            return Err(Error::invalid(
                Reason::Scope,
                "scope identity and parents must be valid",
            ));
        }
        if scope.resources.iter().any(|id| {
            inner
                .resources
                .get(id)
                .is_none_or(|resource| resource.owner != scope.owner)
        }) {
            return Err(Error::invalid(
                Reason::ScopeResource,
                "scope must own its declared resources",
            ));
        }
        if !matches!(scope.owner, Owner::Object(_)) {
            for reference in &scope.dependencies {
                if let Some(target) = inner.references.get(reference).map(|claim| claim.object) {
                    inner.scope_dependencies.insert(target, scope.id);
                }
            }
        }
        inner.owner_scopes.insert(scope.owner, scope.id);
        if scope.owner != Owner::Host {
            inner.non_host_scopes += 1;
        }
        for reference in &scope.dependencies {
            inner.reference_scopes.insert(*reference, scope.id);
        }
        inner.facet_scopes.insert(scope.facet, scope.id);
        for parent in &scope.parents {
            inner.scope_children.insert(*parent, scope.id);
        }
        for resource in &scope.resources {
            inner.resource_scopes.insert(*resource, scope.id);
            if !scope.closing {
                inner.resource_claims.insert(*resource, scope.id);
            }
        }
        if let Some(version) = inner
            .facets
            .get(&scope.facet)
            .or_else(|| {
                inner.pending.as_ref().and_then(|pending| {
                    pending
                        .reload
                        .facets
                        .iter()
                        .find(|facet| facet.id == scope.facet)
                })
            })
            .map(|facet| facet.code_version)
        {
            inner.scope_code_versions.insert(scope.id, version);
        }
        if !scope.closing {
            inner.track_scope_version(&scope, true);
        }
        inner.scopes.insert(scope.id, scope);
        Ok(())
    }

    /// Activates a scope, suspending on indexed resource readiness when another program opens a dependency.
    pub fn activate_scope(&self, scope: Id) -> Result<Id> {
        let mut inner = self.inner.borrow_mut();
        if !inner.scopes.contains_key(&scope) {
            return Err(gone(Reason::Scope));
        }
        let id = inner.id()?;
        if let Some(program) = inner.scope_activations.get(&scope) {
            return Ok(*program);
        }
        let mut program = Program::new(vec![scope], Phase::Open);
        program.activation_scope = Some(scope);
        program.activation_owner = Some(inner.scopes[&scope].owner);
        inner.scope_activations.insert(scope, id);
        inner.programs.insert(id, program);
        Ok(id)
    }

    /// Check whether a scope remains registered.
    pub fn scope_exists(&self, id: Id) -> bool {
        self.inner.borrow().scopes.contains_key(&id)
    }

    /// Read only scopes indexed under this owner.
    pub fn scopes_for_owner(&self, owner: Owner) -> Vec<Id> {
        self.inner.borrow().owner_scopes.members(&owner).collect()
    }

    /// Read only scopes indexed under this facet version.
    pub fn scopes_for_facet(&self, facet: Id) -> Vec<Id> {
        self.inner.borrow().facet_scopes.members(&facet).collect()
    }

    /// Attach a same-owner resource and update version claims.
    pub fn add_scope_resource(&self, scope: Id, resource: Id) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        let owner = inner
            .resources
            .get(&resource)
            .ok_or_else(|| gone(Reason::Resource))?
            .owner;
        let scope = inner
            .scopes
            .get_mut(&scope)
            .ok_or_else(|| gone(Reason::Scope))?;
        if scope.owner != owner {
            return Err(conflict(Reason::ScopeOwner));
        }
        if !scope.resources.contains(&resource) {
            scope.resources.push(resource);
            let id = scope.id;
            let claimed = !scope.closing;
            inner.resource_scopes.insert(resource, id);
            if claimed {
                inner.resource_claims.insert(resource, id);
                if let Some(version) = inner.scope_code_versions.get(&id).copied() {
                    inner.resource_versions.insert(resource, version);
                }
            }
        }
        Ok(())
    }
}

impl crate::host::Inner {
    /// Collect unopened scopes reached through failed dependency indexes.
    // Declarations waiting on a failed activation cannot outlive its unopened dependencies.
    pub(super) fn failed_scope_dependents(&self, scopes: &[Id]) -> Vec<Id> {
        let mut pending = scopes.to_vec();
        let mut affected = BTreeSet::new();
        while let Some(id) = pending.pop() {
            if !affected.insert(id) {
                continue;
            }
            if let Some(scope) = self.scopes.get(&id) {
                pending.extend(
                    self.scope_children
                        .members(&id)
                        .filter(|child| !self.scopes[child].active),
                );
                for resource in &scope.resources {
                    for dependent in self.dependents.members(resource) {
                        pending.extend(
                            self.resource_scopes
                                .members(&dependent)
                                .filter(|scope| !self.scopes[scope].active),
                        );
                    }
                }
            }
        }
        affected.into_iter().collect()
    }

    /// Cancel activations and release scope resource claims once.
    pub(crate) fn begin_cleanup(&mut self, scopes: &[Id]) {
        for id in scopes {
            if let Some(program) = self.scope_activations.get(id).copied()
                && let Some(record) = self.programs.get_mut(&program)
            {
                record.error.get_or_insert_with(|| {
                    Error::new(Code::Cancelled, Reason::ScopeClosed, "scope is closing")
                });
                record.running = true;
                self.ready_programs.insert(program);
            }
            let Some(scope) = self.scopes.get(id).cloned() else {
                continue;
            };
            if !scope.closing {
                self.track_scope_version(&scope, false);
                self.scopes.get_mut(id).expect("checked scope").closing = true;
                for resource in scope.resources {
                    self.resource_claims.remove(&resource, *id);
                    if !self.resource_claims.contains(&resource) {
                        self.wake(WaitKey::Resource(resource));
                    }
                }
            }
        }
    }

    /// Update owner and resource version counts with the scope claim.
    pub(super) fn track_scope_version(&mut self, scope: &Scope, add: bool) {
        let Some(version) = self.scope_code_versions.get(&scope.id).copied() else {
            return;
        };
        if add {
            self.scope_versions.insert(scope.owner, version);
        } else {
            self.scope_versions.remove(&scope.owner, version);
        }
        for resource in &scope.resources {
            if add {
                self.resource_versions.insert(*resource, version);
            } else {
                self.resource_versions.remove(resource, version);
            }
        }
    }
}
