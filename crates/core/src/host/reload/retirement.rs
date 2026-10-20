//! Callable epoch retirement and independently retained physical cleanup.
use super::*;
use crate::{Result, RuntimeError, host::HostInner};
use futures::{FutureExt, future::LocalBoxFuture};
use std::rc::Rc;

impl HostInner {
    pub(super) async fn retire_previous(
        self: &Rc<Self>,
        deadline: Duration,
        signal: &CancellationToken,
    ) -> Result<()> {
        let Some(previous) = self.epochs.previous() else {
            return Ok(());
        };
        let version = previous.version.get();
        let host = Rc::downgrade(self);
        let idle: LocalBoxFuture<'static, Result<()>> = async move {
            loop {
                let host = host
                    .upgrade()
                    .ok_or_else(|| RuntimeError::unavailable("closed", "Host ended"))?;
                let changed = host.roots.changed();
                if host.roots.epoch_idle(version) {
                    return Ok(());
                }
                drop(host);
                changed.cancelled().await;
            }
        }
        .boxed_local();
        let stopped =
            futures::future::select(signal.cancelled(), self.driver.sleep(deadline)).boxed_local();
        match futures::future::select(idle, stopped).await {
            futures::future::Either::Left((result, _)) => result?,
            futures::future::Either::Right((futures::future::Either::Left((cause, _)), _)) => {
                return Err(cause);
            }
            futures::future::Either::Right((futures::future::Either::Right(_), _)) => {}
        }
        let retired = self.epochs.retire(&previous)?;
        retired
            .record
            .cancellation
            .cancel(crate::epoch::superseded());
        self.registries.retire_epoch(version);
        let objects = self.objects.borrow().values().cloned().collect::<Vec<_>>();
        for object in objects {
            let removed = object.installed.borrow_mut().remove(&version);
            if let Some(removed) = removed {
                let retained = object
                    .installed
                    .borrow()
                    .values()
                    .cloned()
                    .collect::<Vec<_>>();
                if retained
                    .iter()
                    .any(|installed| Rc::ptr_eq(&installed.declarations, &removed.declarations))
                {
                    continue;
                }
                let done = CancellationToken::new();
                self.retirements
                    .borrow_mut()
                    .retain(|entry| !entry.is_cancelled());
                self.retirements.borrow_mut().push(done.clone());
                let host = self.clone();
                self.driver.spawn(
                    async move {
                        loop {
                            let changed = host.roots.changed();
                            if !object
                                .active_contexts
                                .borrow()
                                .values()
                                .any(|entry| entry.version <= version)
                            {
                                break;
                            }
                            changed.cancelled().await;
                        }
                        let resources = retained
                            .iter()
                            .map(|installed| installed.declarations.resources.clone())
                            .collect::<Vec<_>>();
                        if let Err(error) = removed.declarations.resources.retire(&resources).await
                        {
                            host.errors.borrow_mut().push(error);
                        }
                        let state = retained
                            .iter()
                            .map(|installed| installed.declarations.state.clone())
                            .collect::<Vec<_>>();
                        object
                            .state
                            .retire(&removed.declarations.state, &state)
                            .finish();
                        for reference in removed.declarations.dependencies.values() {
                            if !retained.iter().any(|installed| {
                                installed
                                    .declarations
                                    .dependencies
                                    .values()
                                    .any(|other| other.same_claim(reference))
                            }) {
                                reference.release();
                            }
                        }
                        removed.declarations.activities.clear().start();
                        drop((retained, removed));
                        done.cancel(RuntimeError::aborted(
                            "settled",
                            "retired declaration cleanup settled",
                        ));
                    }
                    .boxed_local(),
                );
            }
        }
        Ok(())
    }
}
