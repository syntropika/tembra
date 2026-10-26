//! Open dependencies and prepare candidate state.

use super::*;

impl Host {
    /// Open only resources whose indexed dependencies are ready.
    pub(super) fn advance_open(
        &self,
        program: &mut Program,
        effects: &mut Vec<Effect>,
    ) -> Result<Option<Step>> {
        let inner = self.inner.borrow();
        if program.scopes.iter().any(|scope| {
            inner.scopes[scope].dependencies.iter().any(|reference| {
                inner
                    .references
                    .get(reference)
                    .filter(|reference| !reference.released)
                    .and_then(|reference| inner.objects.get(&reference.object))
                    .is_none_or(|object| object.phase == crate::Status::Gone)
            })
        }) {
            return Err(Error::new(
                Code::Unavailable,
                Reason::Dependency,
                "scope dependency failed",
            ));
        }
        for id in &program.scopes {
            let scope = &inner.scopes[id];
            for parent in &scope.parents {
                if !inner.scopes[parent].active && !program.scope_set.contains(parent) {
                    program.waiting.insert(WaitKey::Scope(*parent));
                }
            }
            for reference_id in &scope.dependencies {
                if let Some(reference) = inner.references.get(reference_id)
                    && inner
                        .objects
                        .get(&reference.object)
                        .is_some_and(|object| object.phase == crate::Status::Loading)
                {
                    program.waiting.insert(WaitKey::Object(reference.object));
                    program.waiting.insert(WaitKey::Reference(*reference_id));
                }
            }
        }
        if !program.waiting.is_empty() {
            return Ok(Some(Step::Wait {
                deadline: program.deadline,
            }));
        }
        let mut missing = false;
        let mut unique = BTreeSet::new();
        for scope in &program.scopes {
            for id in &inner.scopes[scope].resources {
                let resource = inner
                    .resources
                    .get(id)
                    .filter(|resource| !resource.closing)
                    .ok_or_else(|| gone(Reason::Resource))?;
                if resource.value.is_none() {
                    missing = true;
                    let mut ready = true;
                    for dependency in &resource.dependencies {
                        let record = inner
                            .resources
                            .get(dependency)
                            .filter(|resource| !resource.closing)
                            .ok_or_else(|| gone(Reason::Resource))?;
                        // Retired scopes cannot supply dependencies to new activations.
                        if inner.resource_scopes.contains(dependency)
                            && !inner.resource_claims.contains(dependency)
                        {
                            return Err(gone(Reason::Resource));
                        }
                        if record.value.is_none() {
                            ready = false;
                        }
                    }
                    if ready && unique.insert(*id) {
                        effects.push(Effect::Open { resource: *id });
                    } else if !ready {
                        // A ready sibling dependency may retire while another opens.
                        program.waiting.extend(
                            resource
                                .dependencies
                                .iter()
                                .map(|id| WaitKey::Resource(*id)),
                        );
                    }
                }
            }
        }
        if missing && effects.is_empty() {
            if program.waiting.is_empty() {
                return Err(conflict(Reason::ResourceDependencies));
            }
            return Ok(Some(Step::Wait {
                deadline: program.deadline,
            }));
        }
        if !missing {
            program.phase = Phase::Prepare;
        }
        Ok(None)
    }

    /// Request scope preparation before checking candidate state.
    pub(super) fn advance_prepare(
        &self,
        program: &mut Program,
        effects: &mut Vec<Effect>,
    ) -> Result<Option<Step>> {
        let inner = self.inner.borrow();
        effects.extend(
            program
                .scopes
                .iter()
                .filter(|scope| program.ticket.is_some() || !inner.scopes[scope].active)
                .map(|scope| Effect::Prepare {
                    scope: *scope,
                    previous: program.previous_scopes.get(scope).copied().flatten(),
                }),
        );
        program.phase = Phase::State;
        Ok(None)
    }

    /// Select initialization, migration or validation for candidate state.
    pub(super) fn advance_state(
        &self,
        program: &mut Program,
        effects: &mut Vec<Effect>,
    ) -> Result<Option<Step>> {
        let inner = self.inner.borrow();
        if program.ticket.is_some() {
            for id in &program.scopes {
                let scope = &inner.scopes[id];
                if let (Owner::Object(object), Some(version)) = (scope.owner, scope.state_version) {
                    let snapshot = inner.objects[&object].state.unwrap_or_default();
                    program.expected.insert(*id, snapshot);
                    if snapshot.version == 0 {
                        effects.push(Effect::Initialize { scope: *id });
                    } else if version > snapshot.version {
                        effects.push(Effect::Migrate {
                            scope: *id,
                            value: Some(snapshot.value),
                            version: snapshot.version,
                        });
                    } else {
                        effects.push(Effect::Validate {
                            scope: *id,
                            value: snapshot.value,
                        });
                    }
                }
            }
            program.phase = Phase::Prepared;
        } else {
            program.phase = Phase::Start;
        }
        Ok(None)
    }
}
