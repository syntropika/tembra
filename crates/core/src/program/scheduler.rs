//! Advance ready programs. Waiters wake only on indexed changes.

use super::*;

impl Host {
    /// Advance one program and update its indexed wait memberships.
    pub fn next_step(&self, id: Id) -> Result<Step> {
        let mut inner = self.inner.borrow_mut();
        let mut program = inner
            .programs
            .remove(&id)
            .ok_or_else(|| gone(Reason::Program))?;
        for key in std::mem::take(&mut program.waiting) {
            inner.waiters.remove(&key, id);
        }
        let deadline = program.deadline;
        drop(inner);
        let result = self.advance(id, &mut program);
        if !matches!(&result, Ok(Step::Wait { .. })) {
            program.waiting.clear();
        }
        let mut inner = self.inner.borrow_mut();
        if deadline != program.deadline {
            inner.remove_deadline(deadline, id);
        }
        for key in &program.waiting {
            inner.waiters.insert(*key, id);
        }
        inner.programs.insert(id, program);
        result
    }

    /// Remove a program only after it reaches a terminal phase.
    pub fn forget_program(&self, id: Id) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        if inner
            .programs
            .get(&id)
            .is_some_and(|program| !matches!(program.phase, Phase::Complete | Phase::Failed))
        {
            return Err(conflict(Reason::ProgramRunning));
        }
        inner.remove_program(id);
        Ok(())
    }

    /// Advance phases until callbacks, a wait or a terminal result is required.
    pub(super) fn advance(&self, id: Id, program: &mut Program) -> Result<Step> {
        if !program.pending.is_empty() {
            return Err(conflict(Reason::EffectsRunning));
        }
        loop {
            if program
                .deadline
                .is_some_and(|deadline| self.inner.borrow().now >= deadline)
            {
                program
                    .error
                    .get_or_insert_with(|| conflict(Reason::ReloadDeadline));
                program.deadline = None;
            }
            if program.error.is_some()
                && !program.committed
                && !matches!(
                    program.phase,
                    Phase::AbortStop
                        | Phase::AbortClose
                        | Phase::Restore
                        | Phase::Resume
                        | Phase::AbortDrop
                        | Phase::Failed
                        | Phase::Complete
                        | Phase::Drop
                )
            {
                program.phase = Phase::AbortStop;
            }
            let mut effects = Vec::new();
            let step = match program.phase {
                Phase::Stop => self.advance_stop(id, program, &mut effects)?,
                Phase::Gate => self.advance_gate(program)?,
                Phase::CloseExclusive => self.advance_close_exclusive(program, &mut effects)?,
                Phase::Open => self.advance_open(program, &mut effects)?,
                Phase::Prepare => self.advance_prepare(program, &mut effects)?,
                Phase::State => self.advance_state(program, &mut effects)?,
                Phase::Prepared => {
                    program.deadline = None;
                    return Ok(Step::Prepared);
                }
                Phase::Adopt => self.advance_adopt(program, &mut effects)?,
                Phase::Start | Phase::Resume => self.advance_start(program, &mut effects)?,
                Phase::AbortStop => self.advance_abort_stop(program, &mut effects)?,
                Phase::AbortClose if program.ticket.is_none() => {
                    self.advance_cleanup_close(id, program, &mut effects)?
                }
                Phase::AbortClose => self.advance_abort_close(program, &mut effects)?,
                Phase::Restore => self.advance_restore(program, &mut effects)?,
                Phase::Drop | Phase::AbortDrop => self.advance_drop(program, &mut effects)?,
                Phase::Complete => {
                    return Ok(if let Some(error) = &program.error {
                        Step::Failed(error.clone())
                    } else {
                        Step::Complete
                    });
                }
                Phase::Failed => {
                    return Ok(Step::Failed(
                        program.error.clone().expect("aborted program"),
                    ));
                }
            };
            if let Some(step) = step {
                return Ok(step);
            }
            if !effects.is_empty() {
                for effect in &effects {
                    if let Effect::Stop {
                        scope,
                        keep_listeners: _keep,
                    } = effect
                    {
                        if let Some(scope) = self.inner.borrow_mut().scopes.get_mut(scope) {
                            scope.active = false;
                        }
                        #[cfg(feature = "subscriptions")]
                        self.stop_scope_listeners(*scope, *_keep)?;
                    }
                }
                program.pending = effects.clone();
                return Ok(Step::Execute(effects));
            }
        }
    }

    /// Mark a program runnable without executing language callbacks.
    pub fn run_program(&self, id: Id) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        inner
            .programs
            .get_mut(&id)
            .ok_or_else(|| gone(Reason::Program))?
            .running = true;
        inner.ready_programs.insert(id);
        Ok(())
    }

    /// Advances every runnable program. Adapters have no lifecycle loop or phase transitions.
    pub(crate) fn pump_programs(&self) {
        let ids = std::mem::take(&mut self.inner.borrow_mut().ready_programs);
        for id in ids {
            #[cfg(test)]
            {
                self.inner.borrow_mut().program_visits += 1;
            }
            let cancellation = {
                let mut inner = self.inner.borrow_mut();
                let now = inner.now;
                let program = inner.programs.get_mut(&id).expect("listed program");
                if program.deadline.is_some_and(|deadline| now >= deadline)
                    && program.error.is_none()
                {
                    program.error = Some(conflict(Reason::ReloadDeadline));
                    program.deadline = None;
                    program.running = true;
                }
                if program.error.is_some()
                    && !program.pending.is_empty()
                    && !program.cancelled_effects
                {
                    program.cancelled_effects = true;
                    Some(program.pending.clone())
                } else {
                    None
                }
            };
            if let Some(effects) = cancellation {
                self.inner
                    .borrow_mut()
                    .actions
                    .push_back(crate::Action::CancelEffects {
                        program: id,
                        effects,
                    });
            }
            let runnable = {
                let inner = self.inner.borrow();
                let program = &inner.programs[&id];
                program.running && program.pending.is_empty()
            };
            if !runnable {
                continue;
            }
            let step = match self.next_step(id) {
                Ok(step) => step,
                Err(error) => {
                    let mut inner = self.inner.borrow_mut();
                    let program = inner.programs.get_mut(&id).expect("listed program");
                    program.error.get_or_insert(error);
                    program.phase = if program.committed {
                        Phase::Failed
                    } else {
                        Phase::AbortStop
                    };
                    drop(inner);
                    match self.next_step(id) {
                        Ok(step) => step,
                        Err(error) => Step::Failed(error),
                    }
                }
            };
            let mut inner = self.inner.borrow_mut();
            match step {
                Step::Execute(effects) => {
                    inner.programs.get_mut(&id).expect("listed").running = false;
                    inner.actions.push_back(crate::Action::Execute {
                        program: id,
                        effects,
                    });
                }
                Step::Wait { .. } => {}
                Step::Prepared => {
                    inner.programs.get_mut(&id).expect("listed").running = false;
                    inner.actions.push_back(crate::Action::ProgramPrepared(id));
                }
                Step::Complete => {
                    inner.remove_program(id);
                    inner.actions.push_back(crate::Action::ProgramComplete {
                        program: id,
                        error: None,
                    });
                }
                Step::Failed(error) => {
                    if let Some(Owner::Object(object)) = inner.programs[&id].activation_owner {
                        if let Some(record) = inner.objects.get_mut(&object) {
                            record.phase = crate::Status::Gone;
                            record.pinned = false;
                        }
                        inner.maybe_dispose(object);
                    }
                    inner.remove_program(id);
                    inner.actions.push_back(crate::Action::ProgramComplete {
                        program: id,
                        error: Some(error),
                    });
                }
            }
        }
    }

    /// Read the delay until the earliest indexed deadline.
    pub fn next_wake(&self) -> Option<u64> {
        let inner = self.inner.borrow();
        self.next_deadline()
            .map(|deadline| deadline.saturating_sub(inner.now))
    }

    /// Absolute monotonic deadline; adapters can retain an unchanged scheduled timer.
    pub fn next_deadline(&self) -> Option<u64> {
        let inner = self.inner.borrow();
        inner
            .program_deadlines
            .keys()
            .next()
            .copied()
            .into_iter()
            .chain(inner.idle.keys().next().copied())
            .min()
    }
}
