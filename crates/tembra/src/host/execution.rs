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
            let result = std::panic::AssertUnwindSafe(operation)
                .catch_unwind()
                .await
                .unwrap_or_else(|_| {
                    Err(Error::new(
                        core::Code::Unavailable,
                        "callback",
                        "native callback panicked",
                    ))
                });
            let _ = send.send(result);
            host.0.active.set(host.0.active.get() - 1);
            host.0.settled_waker.wake();
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
            if let Some(wake) = self.0.driver_wake.borrow().clone() {
                wake();
            }
            return;
        }
        if self.0.flushing.replace(true) {
            return;
        }
        let _guard = FlushGuard(self.clone());
        if self.kernel().next_deadline().is_some() {
            self.tick();
        }
        loop {
            let actions = self.kernel().drain();
            if actions.is_empty() {
                break;
            }
            for action in actions {
                match action {
                    #[cfg(feature = "subscriptions")]
                    core::Action::Deliver {
                        subscription,
                        operation,
                        payload,
                    } => {
                        self.deliver(subscription, operation, payload);
                    }
                    #[cfg(feature = "subscriptions")]
                    core::Action::DetachSubscription(subscription) => {
                        self.detach_listener(subscription);
                    }
                    #[cfg(feature = "subscriptions")]
                    core::Action::SubscriptionFailed(subscription) => {
                        self.detach_listener(subscription);
                    }
                    core::Action::StateChanged { object, .. } => self.publish_state(object),
                    core::Action::CancelTask(task) => self.cancel_task(task),
                    core::Action::CancelRoot(root) => self.cancel_context(root),
                    core::Action::DropValue(id) => self.drop_value(id),
                    core::Action::DisposeObject(id) => {
                        self.0.observers.borrow_mut().remove(&id);
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
                            if !host.0.external_actions.get() {
                                host.tick();
                            }
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
                    core::Action::RetireOrb(id) => {
                        self.0.orbs.borrow_mut().remove(&id);
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
        if let Some(signal) = self.0.cancellations.borrow_mut().remove(&resource) {
            signal.cancel();
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
                if let Some(record) = self.0.scopes.borrow_mut().remove(scope) {
                    if record.owner == core::Owner::Host {
                        let resources = record.draft.resources.borrow();
                        self.0
                            .host_resources
                            .borrow_mut()
                            .retain(|_, key| !resources.contains(&key.id));
                    }
                    if self.0.orb_scopes.borrow().get(&record.orb) == Some(scope) {
                        self.0.orb_scopes.borrow_mut().remove(&record.orb);
                        self.0.orb_values.borrow_mut().remove(&record.orb);
                    }
                }
                self.0.scope_activations.borrow_mut().remove(scope);
                if let Some(object) = self.0.scope_objects.borrow_mut().remove(scope) {
                    self.0.objects.borrow_mut().remove(&object);
                }
            }
            core::Effect::Stop { scope, .. } => self.stop_scope(*scope)?,
            core::Effect::Start { scope } => self.start_scope(*scope)?,
            core::Effect::Prepare { scope, .. } => {
                #[cfg(feature = "subscriptions")]
                self.prepare_listeners(*scope)?;
                #[cfg(not(feature = "subscriptions"))]
                let _ = scope;
            }
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

struct FlushGuard(Host);
impl Drop for FlushGuard {
    fn drop(&mut self) {
        self.0.0.flushing.set(false);
        self.0.schedule_deadline();
    }
}
