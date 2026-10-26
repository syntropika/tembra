//! Record settled effects before the next lifecycle phase.

use super::*;

impl Host {
    /// Acknowledges only physical execution settlement. Observer cancellation is not completion.
    pub fn complete_step(&self, id: Id, results: Vec<Completion>) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        #[cfg(feature = "subscriptions")]
        let mut started = Vec::new();
        let crate::host::Inner {
            programs,
            scopes,
            actions,
            ..
        } = &mut *inner;
        let program = programs.get_mut(&id).ok_or_else(|| gone(Reason::Program))?;
        if program.pending.len() != results.len() || results.is_empty() {
            return Err(conflict(Reason::EffectResults));
        }
        // Invalid results still acknowledge physical settlement of the full batch.
        for (effect, result) in std::mem::take(&mut program.pending)
            .into_iter()
            .zip(results)
        {
            if let Some(error) = result.error {
                program.error.get_or_insert(error);
                continue;
            }
            #[cfg(feature = "subscriptions")]
            if let Effect::Start { scope } = effect {
                started.push(scope);
            }
            if let Effect::Migrate { scope, .. } | Effect::Initialize { scope } = effect {
                let Some(value) = result.value else {
                    program.error.get_or_insert_with(|| {
                        Error::invalid(
                            Reason::MigrationValue,
                            "migration must return a value identity",
                        )
                    });
                    continue;
                };
                let snapshot = program.expected[&scope];
                let declared = &scopes[&scope];
                let Owner::Object(object) = declared.owner else {
                    actions.push_back(crate::Action::DropValue(value));
                    program
                        .error
                        .get_or_insert_with(|| conflict(Reason::StateOwner));
                    continue;
                };
                program.migrations.push(Migration {
                    object,
                    expected_revision: snapshot.revision,
                    value,
                    state_version: declared.state_version.expect("state declaration"),
                });
            }
        }
        program.running = true;
        inner.ready_programs.insert(id);
        #[cfg(feature = "subscriptions")]
        {
            drop(inner);
            for scope in started {
                self.start_scope_listeners(scope);
            }
        }
        Ok(())
    }
}
