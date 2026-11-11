//! Native callback storage follows the kernel's existing replacement transaction.
use super::*;

impl Host {
    /// Replace implementations through kernel-owned preparation, publication and physical cleanup.
    /// The operation remains owned by the host if its observer stops waiting.
    pub async fn reload(&self, orbs: Vec<Orb>, deadline_ms: u64) -> Result<core::Version> {
        if self.0.external_actions.get() {
            return Err(Error::invalid(
                Reason::Driver,
                "the shared JS adapter owns replacement",
            ));
        }
        let host = self.clone();
        self.spawn(async move { host.replace(orbs, deadline_ms).await })?
            .await
            .map_err(|_| Error::invalid(Reason::Executor, "replacement executor stopped"))?
    }

    /// Resolve candidate metadata only while its callbacks are being captured.
    pub(super) fn orb_info(&self, id: core::Id) -> Result<core::InstalledOrb> {
        self.0
            .pending_orbs
            .borrow()
            .get(&id)
            .cloned()
            .map_or_else(|| self.kernel().orb(id), Ok)
    }

    /// Capture native environments before asking the kernel to run replacement effects.
    async fn replace(&self, orbs: Vec<Orb>, deadline_ms: u64) -> Result<core::Version> {
        let current = self.kernel().installed_orbs();
        let replacing = current
            .iter()
            .filter(|info| {
                orbs.iter()
                    .any(|orb| orb.metadata.name == info.declaration.name)
            })
            .map(|info| info.id)
            .collect::<BTreeSet<_>>();
        let services = current
            .iter()
            .filter(|info| replacing.contains(&info.id))
            .flat_map(|info| {
                info.declaration
                    .services
                    .iter()
                    .map(|service| service.name.as_str())
            })
            .collect::<BTreeSet<_>>();
        for (id, object) in self.0.objects.borrow().iter() {
            if object
                .activation
                .as_ref()
                .is_some_and(|ready| ready.peek().is_none())
                && services.contains(self.kernel().object_address(*id)?.service.as_str())
            {
                return Err(Error::new(
                    core::Code::Conflict,
                    "activation_pending",
                    "wait for pending object activation before replacing its provider",
                ));
            }
        }
        if self.0.scopes.borrow().values().any(|scope| {
            replacing.contains(&scope.orb)
                && scope
                    .tasks
                    .iter()
                    .any(|task| self.0.task_signals.borrow().contains_key(task))
        }) {
            return Err(Error::new(
                core::Code::Unsupported,
                "task_restart",
                "reload requires restartable task declarations while native one-shot tasks are active",
            ));
        }
        #[cfg(feature = "hooks")]
        self.validate_hooks(&orbs)?;
        let reload = self.kernel().prepare(
            orbs.iter().map(|orb| orb.metadata.clone()).collect(),
            vec![],
        )?;
        let ids = reload.orbs.iter().map(|orb| orb.id).collect::<Vec<_>>();
        self.0
            .pending_orbs
            .borrow_mut()
            .extend(reload.orbs.iter().map(|orb| (orb.id, orb.clone())));
        #[cfg(feature = "hooks")]
        let mut hooks = Vec::new();
        for orb in orbs {
            let info = reload
                .orbs
                .iter()
                .find(|info| info.declaration.name == orb.metadata.name)
                .ok_or_else(|| {
                    Error::invalid(Reason::Orb, "prepared candidate identity is missing")
                })?;
            self.0
                .orb_factories
                .borrow_mut()
                .insert(info.id, orb.factory);
            self.0.orbs.borrow_mut().insert(info.id, orb.services);
            #[cfg(feature = "hooks")]
            hooks.push((info.id, orb.hooks, orb.contributions));
        }
        let capture = (|| {
            for id in &ids {
                self.ensure_orb(*id)?;
            }
            for object in &reload.objects {
                if !self.0.objects.borrow().contains_key(object) {
                    continue;
                }
                let address = self.kernel().object_address(*object)?;
                let Some(orb) = reload.providers.get(&address.service) else {
                    continue;
                };
                if ids.contains(orb) {
                    self.activate(*object, *orb, &address.service)?;
                }
            }
            // The deadline counts from now, not from the kernel's last observed time.
            self.tick();
            self.kernel()
                .reload_registered_scopes(reload.id, deadline_ms)
        })();
        for id in &ids {
            self.0.pending_orbs.borrow_mut().remove(id);
        }
        let program = match capture {
            Ok(program) => program,
            Err(error) => {
                self.kernel().abort(reload.id)?;
                self.discard_candidates(&ids).await?;
                return Err(error);
            }
        };
        let (prepared, receive) = oneshot::channel();
        self.0
            .prepared_programs
            .borrow_mut()
            .insert(program, prepared);
        let complete = self.run(program)?;
        let prepared = receive
            .await
            .map_err(|_| Error::invalid(Reason::Executor, "replacement preparation stopped"))?;
        if let Err(error) = prepared {
            let _ = complete.await;
            self.forget_candidates(&ids);
            return Err(error);
        }
        if let Err(error) = self.kernel().commit_program(program) {
            self.kernel().abort_program(program)?;
            self.flush();
            let _ = complete.await;
            self.forget_candidates(&ids);
            return Err(error);
        }
        #[cfg(feature = "hooks")]
        for (id, declarations, contributions) in hooks {
            self.install_hooks(id, declarations, contributions)?;
        }
        self.flush();
        complete.await?;
        Ok(reload.code_version)
    }

    /// Wait for candidate resource cleanup before releasing their callback storage.
    async fn discard_candidates(&self, ids: &[core::Id]) -> Result<()> {
        let scopes = ids
            .iter()
            .flat_map(|id| self.kernel().scopes_for_orb(*id))
            .collect();
        let program = self.kernel().cleanup_scopes(scopes, None)?;
        let result = self.run(program)?.await;
        if !self
            .0
            .scopes
            .borrow()
            .values()
            .any(|scope| ids.contains(&scope.orb))
        {
            self.forget_candidates(ids);
        }
        result
    }

    /// Remove callbacks for a physically discarded candidate, leaving installed versions intact.
    fn forget_candidates(&self, ids: &[core::Id]) {
        for id in ids {
            self.0.orbs.borrow_mut().remove(id);
            self.0.orb_factories.borrow_mut().remove(id);
            self.0.orb_errors.borrow_mut().remove(id);
            self.0.pending_orbs.borrow_mut().remove(id);
            #[cfg(feature = "hooks")]
            {
                self.0.hooks.declarations.borrow_mut().remove(id);
                self.0
                    .hooks
                    .contributions
                    .borrow_mut()
                    .retain(|_, (orb, _)| orb != id);
            }
        }
    }

    /// Retain old environments until the kernel releases their last version pin.
    pub(super) async fn retire_orb(&self, orb: core::Id) -> Result<()> {
        self.discard_candidates(&[orb]).await
    }
}
