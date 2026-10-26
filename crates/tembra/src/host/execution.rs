//! Execute kernel effects and acknowledge physical settlement.

use super::context::RootGuard;
use super::*;

impl Host {
    /// Retain physical work and wake shutdown waiters after settlement.
    pub(super) fn spawn<T: 'static>(
        &self,
        operation: impl Future<Output = Result<T>> + 'static,
    ) -> Result<oneshot::Receiver<Result<T>>> {
        let (send, receive) = oneshot::channel();
        let host = self.clone();
        self.0.active.set(self.0.active.get() + 1);
        if let Err(error) = self.0.executor.spawn_local(async move {
            let _ = send.send(operation.await);
            host.0.active.set(host.0.active.get() - 1);
            for waiter in host.0.settled.take() {
                let _ = waiter.send(());
            }
        }) {
            self.0.active.set(self.0.active.get() - 1);
            return Err(Error::invalid(Reason::Executor, error.to_string()));
        }
        Ok(receive)
    }

    /// Drain native-owned actions. A shared JS adapter owns its own drain.
    pub fn flush(&self) {
        if self.0.external_actions.get() {
            return;
        }
        loop {
            let actions = self.kernel().drain();
            if actions.is_empty() {
                break;
            }
            for action in actions {
                match action {
                    core::Action::DropValue(id) => self.drop_value(id),
                    core::Action::DisposeObject(id) => {
                        let host = self.clone();
                        if let Err(error) = self.spawn(async move {
                            if let Err(error) = host.dispose(id).await {
                                host.0.errors.borrow_mut().push(error);
                            }
                            host.kernel().disposed(id)?;
                            host.flush();
                            Ok(())
                        }) {
                            self.0.errors.borrow_mut().push(error);
                        }
                    }
                    core::Action::Execute { program, effects } => {
                        // Spawn only user futures. Settle bookkeeping in place.
                        if effects.iter().all(|effect| {
                            !matches!(
                                effect,
                                core::Effect::Close { .. } | core::Effect::Open { .. }
                            )
                        }) {
                            let results = effects
                                .iter()
                                .map(|effect| completion(self.execute_immediate(effect)))
                                .collect();
                            if let Err(error) = self.kernel().complete_step(program, results) {
                                self.0.errors.borrow_mut().push(error);
                            }
                            continue;
                        }
                        let host = self.clone();
                        if let Err(error) = self.spawn(async move {
                            let results = futures::future::join_all(
                                effects.iter().map(|effect| host.execute_effect(effect)),
                            )
                            .await
                            .into_iter()
                            .map(completion)
                            .collect();
                            host.kernel().complete_step(program, results)?;
                            host.flush();
                            Ok(())
                        }) {
                            self.0.errors.borrow_mut().push(error);
                        }
                    }
                    core::Action::ProgramComplete { program, error } => {
                        if let Some(send) = self.0.programs.borrow_mut().remove(&program) {
                            let _ = send.send(error.map_or(Ok(()), Err));
                        }
                    }
                    core::Action::RetireFacet(id) => {
                        self.0.facets.borrow_mut().remove(&id);
                    }
                    core::Action::CancelEffects { effects, .. } => {
                        for effect in effects {
                            self.cancel_effect(&effect);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    /// Await a kernel program without implementing lifecycle policy in the adapter.
    pub(super) fn run(
        &self,
        program: core::Id,
    ) -> Result<Shared<LocalBoxFuture<'static, Result<()>>>> {
        let (send, receive) = oneshot::channel();
        self.0.programs.borrow_mut().insert(program, send);
        self.kernel().run_program(program)?;
        self.flush();
        Ok(async move {
            receive
                .await
                .map_err(|_| Error::invalid(Reason::Executor, "engine completion was dropped"))?
        }
        .boxed_local()
        .shared())
    }

    /// Signal opener cancellation without acknowledging physical settlement.
    pub fn cancel_effect(&self, effect: &core::Effect) {
        if let core::Effect::Open { resource } | core::Effect::Restore { resource, .. } = effect {
            self.cancel_resource(*resource);
        }
    }

    /// Signal a pending native opener without claiming it has settled.
    pub(super) fn cancel_resource(&self, resource: core::Id) {
        if let Some((signal, send)) = self.0.cancellations.borrow_mut().remove(&resource) {
            signal.cancelled.set(true);
            let _ = send.send(());
        }
    }

    /// Execute physical resource callbacks requested by the kernel.
    pub async fn execute_effect(&self, effect: &core::Effect) -> Result<Option<core::Id>> {
        if let core::Effect::Open { resource } = effect {
            let open = self.0.openers.borrow_mut().remove(resource);
            if let Some(open) = open {
                open().await?;
            } else {
                self.kernel().resource_value(*resource)?;
            }
        } else if let core::Effect::Close { resource } = effect {
            self.cancel_resource(*resource);
            self.0.openers.borrow_mut().remove(resource);
            let close = self.0.closers.borrow_mut().remove(resource);
            let result = if let Some(close) = close {
                let root = self.kernel().cleanup_root("host")?;
                let _guard = RootGuard {
                    host: self.clone(),
                    root,
                };
                close(self.context(root, None)?).await
            } else {
                Ok(())
            };
            self.kernel().resource_closed(*resource)?;
            result?;
        } else {
            return self.execute_immediate(effect);
        }
        Ok(None)
    }

    /// Apply native bookkeeping effects without spawning a user future.
    pub(super) fn execute_immediate(&self, effect: &core::Effect) -> Result<Option<core::Id>> {
        match effect {
            core::Effect::Drop { scope } => {
                if let Some(object) = self.0.scope_objects.borrow_mut().remove(scope) {
                    self.0.objects.borrow_mut().remove(&object);
                }
            }
            core::Effect::Stop { .. }
            | core::Effect::Prepare { .. }
            | core::Effect::Start { .. } => {}
            _ => {
                return Err(Error::invalid(
                    Reason::NativeEffect,
                    "effect has no registered native callback",
                ));
            }
        }
        Ok(None)
    }
}

/// Convert a callback result into kernel completion metadata.
fn completion(result: Result<Option<core::Id>>) -> core::Completion {
    match result {
        Ok(value) => core::Completion { value, error: None },
        Err(error) => core::Completion {
            value: None,
            error: Some(error),
        },
    }
}
