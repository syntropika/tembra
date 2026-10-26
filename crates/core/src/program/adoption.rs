//! Gate and publish replacements without re-entering user code.

use super::*;

impl Host {
    /// Stop old scopes after overlapping replacement leases settle.
    pub(super) fn advance_stop(
        &self,
        id: Id,
        program: &mut Program,
        effects: &mut Vec<Effect>,
    ) -> Result<Option<Step>> {
        if program.ticket.is_none() && self.wait_for_reloads(id, program) {
            return Ok(Some(Step::Wait { deadline: None }));
        }
        if program.ticket.is_some() {
            effects.extend(program.replacements.iter().filter_map(|pair| {
                pair.previous.map(|scope| Effect::Stop {
                    scope,
                    keep_listeners: pair.next.is_some(),
                })
            }));
            program.phase = Phase::Gate;
        } else {
            effects.extend(program.scopes.iter().map(|scope| Effect::Stop {
                scope: *scope,
                keep_listeners: false,
            }));
            program.phase = Phase::AbortClose;
        }
        Ok(None)
    }

    /// Wait for admitted owners before closing exclusive resources.
    pub(super) fn advance_gate(&self, program: &mut Program) -> Result<Option<Step>> {
        let ticket = program.ticket.expect("reload");
        if !self.gate(ticket, &program.objects)? || !self.gate_orbs(ticket, &program.orbs)? {
            program.waiting.extend(
                program
                    .objects
                    .iter()
                    .map(|id| WaitKey::Owner(Owner::Object(*id))),
            );
            program.waiting.extend(
                program
                    .orbs
                    .iter()
                    .map(|id| WaitKey::Owner(Owner::Orb(*id))),
            );
            return Ok(Some(Step::Wait {
                deadline: program.deadline,
            }));
        }
        program.phase = Phase::CloseExclusive;
        Ok(None)
    }

    /// Close exclusive resources before opening their replacements.
    pub(super) fn advance_close_exclusive(
        &self,
        program: &mut Program,
        effects: &mut Vec<Effect>,
    ) -> Result<Option<Step>> {
        let ids = program
            .exclusive
            .iter()
            .map(|resource| resource.id)
            .collect();
        match self.closers(&ids, &BTreeSet::new(), None, &mut program.waiting)? {
            Some(ready) => {
                effects.extend(ready.into_iter().map(|resource| Effect::Close { resource }))
            }
            None => {
                return Ok(Some(Step::Wait {
                    deadline: program.deadline,
                }));
            }
        }
        if effects.is_empty() {
            program.phase = Phase::Open;
        }
        Ok(None)
    }

    /// Publish the prepared replacement before starting its new scopes.
    pub(super) fn advance_adopt(
        &self,
        program: &mut Program,
        effects: &mut Vec<Effect>,
    ) -> Result<Option<Step>> {
        // Rollback is no longer possible. Reused connections retain their next scope.
        #[cfg(feature = "subscriptions")]
        for pair in &program.replacements {
            if let Some(scope) = pair.previous {
                self.stop_scope_listeners(scope, false)?;
            }
        }
        effects.push(Effect::Adopt {
            replacements: program.replacements.clone(),
        });
        program.phase = Phase::Start;
        Ok(None)
    }

    /// Mark adopted or restored scopes active and request their callbacks.
    pub(super) fn advance_start(
        &self,
        program: &mut Program,
        effects: &mut Vec<Effect>,
    ) -> Result<Option<Step>> {
        if self.inner.borrow().closing {
            program.phase = if program.phase == Phase::Resume {
                Phase::AbortDrop
            } else {
                Phase::Complete
            };
            return Ok(None);
        }
        if program.ticket.is_none() {
            let objects: Vec<_> = {
                let inner = self.inner.borrow();
                program
                    .scopes
                    .iter()
                    .filter_map(|scope| match inner.scopes[scope].owner {
                        Owner::Object(object)
                            if inner.objects.get(&object).is_some_and(|record| {
                                record.phase == crate::Status::Loading && !record.disposing
                            }) =>
                        {
                            Some(object)
                        }
                        _ => None,
                    })
                    .collect()
            };
            for object in objects {
                self.ready(object)?;
            }
        }
        let mut inner = self.inner.borrow_mut();
        let scopes: Vec<_> = if program.phase == Phase::Resume {
            program
                .replacements
                .iter()
                .filter_map(|pair| pair.previous)
                .collect()
        } else {
            program.scopes.clone()
        };
        for scope in scopes {
            let record = inner
                .scopes
                .get_mut(&scope)
                .ok_or_else(|| gone(Reason::Scope))?;
            if !record.active {
                record.active = true;
                effects.push(Effect::Start { scope });
                inner.wake(WaitKey::Scope(scope));
            }
        }
        program.phase = if program.phase == Phase::Resume {
            Phase::AbortDrop
        } else {
            Phase::Complete
        };
        Ok(None)
    }
}
