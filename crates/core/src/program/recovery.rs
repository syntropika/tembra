//! Close failed candidates and restore previous resources.

use super::*;

impl Host {
    /// Stop failed candidates and their unopened dependents.
    pub(super) fn advance_abort_stop(
        &self,
        program: &mut Program,
        effects: &mut Vec<Effect>,
    ) -> Result<Option<Step>> {
        if program.ticket.is_none() {
            let mut inner = self.inner.borrow_mut();
            program.scopes = inner.failed_scope_dependents(&program.scopes);
            program.scope_set = program.scopes.iter().copied().collect();
            inner.begin_cleanup(&program.scopes);
        }
        effects.extend(program.scopes.iter().map(|scope| Effect::Stop {
            scope: *scope,
            keep_listeners: false,
        }));
        program.phase = Phase::AbortClose;
        Ok(None)
    }

    /// Close and drop the next dependency-safe cleanup batch.
    pub(super) fn advance_cleanup_close(
        &self,
        id: Id,
        program: &mut Program,
        effects: &mut Vec<Effect>,
    ) -> Result<Option<Step>> {
        match self.cleanup_batch(id, program)? {
            Some(batch) => *effects = batch,
            None => return Ok(Some(Step::Wait { deadline: None })),
        }
        if program.scopes.is_empty() {
            program.phase = Phase::Drop;
        }
        Ok(None)
    }

    /// Close candidate resources before restoring the previous scopes.
    pub(super) fn advance_abort_close(
        &self,
        program: &mut Program,
        effects: &mut Vec<Effect>,
    ) -> Result<Option<Step>> {
        let ids = self.scope_resources(&program.scopes);
        match self.closers(&ids, &program.retained, None, &mut program.waiting)? {
            Some(ready) => {
                effects.extend(ready.into_iter().map(|resource| Effect::Close { resource }))
            }
            None => return Ok(Some(Step::Wait { deadline: None })),
        }
        if effects.is_empty() {
            program.phase = if program.ticket.is_some() {
                Phase::Restore
            } else {
                Phase::Drop
            };
        }
        Ok(None)
    }

    /// Reopen previous exclusive resources in dependency order.
    pub(super) fn advance_restore(
        &self,
        program: &mut Program,
        effects: &mut Vec<Effect>,
    ) -> Result<Option<Step>> {
        if self.inner.borrow().closing {
            self.abort(program.ticket.expect("reload"))?;
            program.phase = Phase::AbortDrop;
            return Ok(None);
        }
        for old in &program.exclusive {
            if self.inner.borrow().resources.contains_key(&old.id)
                || program.restored.contains_key(&old.id)
            {
                continue;
            }
            let dependencies: Vec<_> = old
                .dependencies
                .iter()
                .map(|id| program.restored.get(id).copied().unwrap_or(*id))
                .collect();
            if dependencies.iter().any(|id| {
                self.inner
                    .borrow()
                    .resources
                    .get(id)
                    .is_none_or(|resource| resource.value.is_none())
            }) {
                continue;
            }
            let restored = self.resource(
                old.owner,
                old.name.clone(),
                old.version,
                old.exclusive,
                dependencies,
            )?;
            self.inner
                .borrow_mut()
                .resources
                .get_mut(&restored.id)
                .expect("restored")
                .code_version = old.code_version;
            self.replace_resource(old.id, restored.id);
            program.restored.insert(old.id, restored.id);
            effects.push(Effect::Restore {
                previous: old.id,
                resource: restored.id,
            });
        }
        if effects.is_empty() {
            program.phase = Phase::Resume;
            self.abort(program.ticket.expect("reload"))?;
            for pair in &program.replacements {
                if let Some(previous) = pair.previous
                    && let Some(scope) = self.inner.borrow_mut().scopes.get_mut(&previous)
                {
                    scope.active = false;
                    effects.push(Effect::Prepare {
                        scope: previous,
                        previous: Some(previous),
                    });
                }
            }
        }
        Ok(None)
    }

    /// Release candidate scopes and unused migration values.
    pub(super) fn advance_drop(
        &self,
        program: &mut Program,
        effects: &mut Vec<Effect>,
    ) -> Result<Option<Step>> {
        if program.ticket.is_some() && !program.committed {
            for migration in &program.migrations {
                self.inner
                    .borrow_mut()
                    .actions
                    .push_back(crate::Action::DropValue(migration.value));
            }
            program.migrations.clear();
        }
        for scope in &program.scopes {
            if self.drop_scope(*scope)? {
                effects.push(Effect::Drop { scope: *scope });
            }
        }
        if let Some(object) = program.object.take() {
            self.disposed(object)?;
        }
        program.phase = if program.ticket.is_some() {
            Phase::Failed
        } else {
            Phase::Complete
        };
        Ok(None)
    }
}
