use super::*;
use crate::{
    ErrorCode, ExecutableFacet, Host, Implementation, Result, RuntimeError,
    activity::PreparedActivities,
    epoch::EpochRecord,
    host::{HostInner, facets::PreparedFacetBatch},
    object::{ObjectRecord, Phase},
    state::CandidateSources,
};
use futures::{FutureExt, channel::oneshot, future::LocalBoxFuture};
use std::rc::Rc;

pub(super) struct PendingActivities(pub(super) Option<PreparedActivities>);
impl PendingActivities {
    pub(super) fn get_mut(&mut self) -> &mut PreparedActivities {
        self.0.as_mut().expect("activity candidate retained")
    }
}
impl Drop for PendingActivities {
    fn drop(&mut self) {
        if let Some(candidate) = self.0.take() {
            candidate.abort().start();
        }
    }
}

pub(super) struct CommitPlan {
    pub(super) bindings: super::ReadyBindings,
    pub(super) graphs: Vec<crate::state::PreparedGraphReload>,
    pub(super) tables: crate::registry::store::PreparedRegistryEpoch,
    pub(super) prepared: Vec<(Rc<InstalledVersion>, PendingActivities)>,
    pub(super) removed_objects: Vec<u64>,
    pub(super) barrier: Option<crate::state::ReadyBarrier>,
}

pub(crate) struct PreparedReload {
    data: Option<ReadyData>,
    decided: bool,
    adopted: bool,
}
struct ReadyData {
    host: Rc<HostInner>,
    next: Rc<EpochRecord>,
    signal: CancellationToken,
    batch: PreparedFacetBatch,
    replays: Vec<Replay>,
    gate: Option<crate::rpc::roots::RootGate>,
    plan: CommitPlan,
    pins: Vec<crate::Reference>,
    control: Rc<ReloadControl>,
}

pub(super) struct Replay {
    pub(super) object: Rc<ObjectRecord>,
    pub(super) fresh: bool,
    pub(super) prior: Rc<InstalledVersion>,
    pub(super) candidate: Rc<CandidateDeclarations>,
    pub(super) implementation: Rc<Implementation>,
    pub(super) activity_failure: Option<crate::activity::ActivityFailureCheckpoint>,
}
pub(super) fn claim_retires(replays: &[Replay], claim: &Rc<crate::reference::Claim>) -> bool {
    claim.fixed_binding()
        || replays.iter().any(|replay| {
            replay
                .prior
                .declarations
                .dependencies
                .values()
                .any(|reference| Rc::ptr_eq(&reference.claim, claim))
                && !replay
                    .candidate
                    .dependencies
                    .borrow()
                    .values()
                    .any(|reference| Rc::ptr_eq(&reference.claim, claim))
        })
}
impl Replay {
    fn refuse_fresh(&self, cause: &RuntimeError) {
        if !self.fresh {
            return;
        }
        *self.object.error.borrow_mut() = Some(cause.clone());
        self.object.phase.set(Phase::Failed);
        self.object.host_pin.set(false);
        let claims = self
            .object
            .claims
            .borrow()
            .iter()
            .filter_map(std::rc::Weak::upgrade)
            .collect::<Vec<_>>();
        for claim in claims {
            claim.mark_gone(cause.clone());
        }
        self.object.finish_ready(Err(cause.clone()));
        let factory = self.object.factory.borrow_mut().take();
        let service = self.object.activation_service.borrow_mut().take();
        drop((factory, service));
    }
}
impl Host {
    /// Submits a driver-owned foreign-facet transaction at call time.
    ///
    /// Dropping the observer does not abort admitted work. Before the commit decision,
    /// refusal preserves installed code/state; setup, resource callbacks and stop signals
    /// can already have external effects. Explicit cancellation is cooperative and resource
    /// cleanup waits for callbacks that actually started. Empty input advances the epoch
    /// through unchanged aliases. Native facets cannot be replaced.
    /// The previous callable epoch is retired before preparing a third one. Its roots may
    /// have received `superseded` even when this new transaction subsequently refuses.
    ///
    /// # Errors
    /// Refuses closed/failed Hosts, concurrent reloads, invalid definitions, candidate
    /// contracts/state/resources, gates that miss their deadline and restoration failures.
    /// A live trusted hierarchy requires coordinated reload (`reload_coordination`);
    /// untrusted local replacement of an attached child is refused as `reload_parent`.
    /// No error promises rollback of arbitrary setup/open/close effects or stop signals.
    ///
    /// ```
    /// use std::time::Duration;
    /// use serde_json::json;
    /// use tembra_core::{Host, HostOptions, ManualDriver, ReloadOptions};
    /// let driver = ManualDriver::new();
    /// let host = Host::new(HostOptions::new("reload-example", json!(null)), driver.clone());
    /// let report = driver.run_until(host.reload(vec![], ReloadOptions::default()))?;
    /// assert_eq!(report.current_version(), 2); // Empty reload preserves unchanged aliases.
    /// let invalid = ReloadOptions {
    ///     gate_deadline: Duration::ZERO,
    ///     ..Default::default()
    /// };
    /// assert_eq!(driver.run_until(host.reload(vec![], invalid)).unwrap_err().reason,
    ///     "reload_options");
    /// assert_eq!(driver.run_until(host.root(|ctx| Box::pin(async move {
    ///     Ok(ctx.code_version())
    /// })))?, 2); // Invalid deadlines refuse before another epoch is allocated.
    /// driver.run_until(host.close())?;
    /// # Ok::<(), tembra_core::RuntimeError>(())
    /// ```
    pub fn reload(
        &self,
        definitions: Vec<ExecutableFacet>,
        options: ReloadOptions,
    ) -> LocalBoxFuture<'static, Result<ReloadReport>> {
        if self.inner.requires_reload_coordination() {
            return async {
                Err(RuntimeError::new(
                    ErrorCode::Forbidden,
                    "reload_coordination",
                    concat!(
                        "local reload requires cluster coordination ",
                        "while trusted hierarchy links are active",
                    ),
                ))
            }
            .boxed_local();
        }
        let prepare = self.inner.prepare_reload_at(definitions, options, None);
        let (send, receive) = oneshot::channel();
        self.inner.driver.spawn(
            async move {
                let result = async {
                    let mut prepared = prepare.await?;
                    prepared.decide()?;
                    let report = prepared.adopt()?;
                    prepared.finish(report).await
                }
                .await;
                let _ = send.send(result);
            }
            .boxed_local(),
        );
        async move {
            receive
                .await
                .unwrap_or_else(|_| Err(RuntimeError::unavailable("reload", "reload owner ended")))
        }
        .boxed_local()
    }
}
impl HostInner {
    pub(crate) fn prepare_reload_at(
        self: &Rc<Self>,
        definitions: Vec<ExecutableFacet>,
        options: ReloadOptions,
        target: Option<u64>,
    ) -> LocalBoxFuture<'static, Result<PreparedReload>> {
        if self.closing.get() {
            return async { Err(RuntimeError::unavailable("closed", "Host is closing")) }
                .boxed_local();
        }
        if let Err(error) = options.validate(self.driver.now()) {
            return async move { Err(error) }.boxed_local();
        }
        if let Some(target) = target {
            if let Err(error) = crate::epoch::CodeEpoch::new(target) {
                return async move { Err(error) }.boxed_local();
            }
            if target <= self.epochs.current().version.get() {
                return async {
                    Err(RuntimeError::invalid(
                        "code_version",
                        "trusted target must advance current code",
                    ))
                }
                .boxed_local();
            }
        }
        if target.is_none() && self.has_reload_parent() {
            return async {
                Err(RuntimeError::new(
                    ErrorCode::Forbidden,
                    "reload_parent",
                    "a child with an active trusted parent requires coordinated reload authority",
                ))
            }
            .boxed_local();
        }
        if let Err(error) = self.epochs.reserve_slot() {
            return async move { Err(error) }.boxed_local();
        }
        let control = Rc::new(ReloadControl {
            cancellation: options
                .cancellation
                .as_ref()
                .map(CancellationToken::child)
                .unwrap_or_default(),
            settled: CancellationToken::new(),
        });
        *self.reload_control.borrow_mut() = Some(control.clone());
        let host = self.clone();
        let (send, receive) = oneshot::channel();
        self.driver.spawn(
            async move {
                let result = async {
                    control.cancellation.check()?;
                    if target.is_some()
                        && host.has_reload_parent()
                        && let Some(previous) = host.epochs.previous()
                        && host.retire_permission.get() < previous.version.get()
                    {
                        return Err(RuntimeError::new(
                            ErrorCode::Forbidden,
                            "reload_retire",
                            "trusted parent has not permitted previous epoch retirement",
                        ));
                    }
                    host.retire_previous(options.retire_deadline, &control.cancellation)
                        .await?;
                    control.cancellation.check()?;
                    let next = host.epochs.reserve_target(target)?;
                    host.clone()
                        .prepare_reserved_reload(definitions, options, next, control.clone())
                        .await
                }
                .await;
                if result.is_err() {
                    host.finish_reload_control(&control);
                }
                let _ = send.send(result);
            }
            .boxed_local(),
        );
        async move {
            receive.await.unwrap_or_else(|_| {
                Err(RuntimeError::unavailable(
                    "reload",
                    "reload preparation ended",
                ))
            })
        }
        .boxed_local()
    }
    pub(crate) fn grant_reload_retirement(&self, version: u64) -> Result<()> {
        crate::epoch::CodeEpoch::new(version)?;
        if version >= self.epochs.current().version.get() {
            return Err(RuntimeError::invalid(
                "code_version",
                "parent retirement must name a previous epoch",
            ));
        }
        self.retire_permission
            .set(self.retire_permission.get().max(version));
        Ok(())
    }

    fn release_reload_ready(&self) {
        if let Some(changed) = self.take_reload_ready_notification() {
            changed.cancel(RuntimeError::aborted(
                "reload_ready",
                "coherent reload decision released publication",
            ));
        }
    }
    pub(super) fn take_reload_ready_notification(&self) -> Option<CancellationToken> {
        self.reload_ready
            .replace(false)
            .then(|| self.reload_ready_changed.replace(CancellationToken::new()))
    }
    pub(crate) fn check_reload_binding(&self, object: u64, fingerprint: &str) -> Result<()> {
        if self.reload_ready.get()
            && !self
                .reload_bindings
                .borrow()
                .as_ref()
                .is_some_and(|bindings| {
                    !bindings.blocked.contains(&object)
                        && (!bindings.affected.contains(&object)
                            || bindings
                                .views
                                .contains_key(&(object, fingerprint.to_owned())))
                })
        {
            return Err(RuntimeError::new(
                ErrorCode::UpgradeConflict,
                "upgrade_conflict",
                "consumer profile was not checked for the pending code",
            ));
        }
        Ok(())
    }
    pub(crate) fn ready_binding_view(
        &self,
        object: u64,
        version: u64,
        fingerprint: &str,
    ) -> Option<Rc<Implementation>> {
        self.reload_bindings
            .borrow()
            .as_ref()
            .filter(|bindings| bindings.version == version)
            .and_then(|bindings| {
                bindings
                    .views
                    .get(&(object, fingerprint.to_owned()))
                    .cloned()
            })
    }
    fn finish_reload_control(&self, expected: &Rc<ReloadControl>) {
        let retired = {
            let mut current = self.reload_control.borrow_mut();
            if current
                .as_ref()
                .is_some_and(|current| Rc::ptr_eq(current, expected))
            {
                current.take()
            } else {
                None
            }
        };
        if retired.is_some() {
            let views = self.reload_bindings.borrow_mut().take();
            drop(views);
            self.reload_gating.borrow_mut().clear();
            self.epochs.release_slot();
        }
        drop(retired);
        expected.settled.cancel(RuntimeError::aborted(
            "settled",
            "reload physically settled",
        ));
    }

    async fn prepare_reserved_reload(
        self: Rc<Self>,
        definitions: Vec<ExecutableFacet>,
        options: ReloadOptions,
        next: Rc<EpochRecord>,
        control: Rc<ReloadControl>,
    ) -> Result<PreparedReload> {
        let signal = control.cancellation.clone();
        let batch = match self.prepare_reload_facets(definitions, &options.remove) {
            Ok(batch) => batch,
            Err(error) => {
                self.epochs.abort_pending(&next)?;
                return Err(error);
            }
        };
        let mut replays = Vec::new();
        let mut gate = None;
        let catalog = CandidateCatalog::new(&self, &batch, signal.clone(), next.version);
        let result = async {
            self.check_reload_services(&batch)?;
            self.prepare_replays(&batch, &next, &signal, &catalog, &mut replays)
                .await?;
            // Match the source fixpoint boundary: freeze new changed-type activation,
            // then drain objects whose installed activation was already admitted.
            let mut changed = batch
                .services
                .keys()
                .cloned()
                .collect::<std::collections::HashSet<_>>();
            changed.extend(self.reload_removed_services(&batch));
            *self.reload_gating.borrow_mut() = changed;
            self.prepare_replays(&batch, &next, &signal, &catalog, &mut replays)
                .await
        }
        .await;
        let sources = CandidateSources::new(
            replays
                .iter()
                .map(|replay| replay.candidate.graph.clone())
                .collect(),
        );
        let result = async {
            result?;
            let mut task_waits = Vec::new();
            for replay in &replays {
                if let Some(runs) = replay.prior.declarations.activity_runs.borrow().clone() {
                    runs.pause_tasks(RuntimeError::aborted(
                        "reload",
                        "task generation stopping for replacement",
                    ));
                    if !replay.candidate.graph.requires_gate()
                        && !replay.candidate.resources.requires_gate()
                    {
                        task_waits.push(runs.task_receipts_settled());
                    }
                }
            }
            let task_wait = futures::future::join_all(task_waits)
                .map(|_| Ok::<(), RuntimeError>(()))
                .boxed_local();
            let stopped = futures::future::select(
                signal.cancelled(),
                self.driver.sleep(options.task_stop_deadline),
            )
            .boxed_local();
            match futures::future::select(task_wait, stopped).await {
                futures::future::Either::Left(_) => {}
                futures::future::Either::Right((futures::future::Either::Left((cause, _)), _)) => {
                    return Err(cause);
                }
                futures::future::Either::Right((futures::future::Either::Right(_), _)) => {
                    self.errors.borrow_mut().push(RuntimeError::unavailable(
                        "task_stop_deadline",
                        "stopped task work remains physically unsettled after its deadline",
                    ));
                }
            }
            let gated = replays
                .iter()
                .filter(|replay| {
                    replay.candidate.graph.requires_gate()
                        || replay.candidate.resources.requires_gate()
                })
                .map(|replay| replay.object.clone())
                .collect::<Vec<_>>();
            if !gated.is_empty() {
                for replay in &replays {
                    if gated
                        .iter()
                        .any(|object| Rc::ptr_eq(object, &replay.object))
                        && let Some(runs) = replay.prior.declarations.activity_runs.borrow().clone()
                    {
                        runs.pause(RuntimeError::aborted(
                            "reload_gate",
                            "activities paused for reload gate",
                        ));
                    }
                }
                gate = Some(self.roots.begin_gate(&gated)?);
                let wait = gate.as_ref().expect("gate installed").drained();
                self.reload_wait(wait, options.gate_deadline, &signal, "reload_gate")
                    .await?;
            }
            let results = futures::future::join_all(
                replays
                    .iter()
                    .map(|replay| replay.candidate.resources.replace_exclusive(signal.clone())),
            )
            .await;
            results.into_iter().collect::<Result<Vec<_>>>()?;
            let links = catalog.state_links(
                replays
                    .iter()
                    .flat_map(|replay| {
                        replay
                            .candidate
                            .dependencies
                            .borrow()
                            .values()
                            .cloned()
                            .collect::<Vec<_>>()
                    })
                    .collect(),
            )?;
            let sources = sources.with_dependents(Vec::new(), links)?;
            self.reload_wait(
                sources.hydrate_external(signal.clone()),
                self.ready_timeout,
                &signal,
                "ready_timeout",
            )
            .await?;
            sources.prepare_migrations()?;
            self.initialize_replays(&replays, &sources, &signal).await?;
            signal.check()?;
            for replay in &replays {
                if let Some(checkpoint) = &replay.activity_failure {
                    checkpoint.check()?;
                }
            }
            let mut plan = self.prepare_commit(&batch, &replays, &sources, &next)?;
            for (_, activities) in &mut plan.prepared {
                activities.get_mut().seal_ready()?;
            }
            Ok(plan)
        }
        .await;
        match result {
            Err(cause) => {
                signal.cancel(cause.clone());
                batch.abort(cause.clone());
                let cleanup = futures::future::join_all(
                    replays
                        .iter()
                        .map(|replay| replay.candidate.resources.abort(cause.clone())),
                )
                .await;
                for replay in &replays {
                    replay.candidate.graph.abort().finish();
                    replay.refuse_fresh(&cause);
                    for reference in replay.candidate.dependencies.borrow().values() {
                        if !replay
                            .prior
                            .declarations
                            .dependencies
                            .values()
                            .any(|prior| prior.same_claim(reference))
                        {
                            reference.release();
                        }
                    }
                }
                drop(gate.take());
                for replay in &replays {
                    if replay.candidate.resources.installed_available()
                        && let Some(runs) = replay.prior.declarations.activity_runs.borrow().clone()
                    {
                        runs.resume_after_abort().start();
                    }
                }
                self.epochs.abort_pending(&next)?;
                for error in cleanup.into_iter().filter_map(Result::err) {
                    self.errors.borrow_mut().push(error);
                }
                Err(cause)
            }
            Ok(plan) => {
                let views = plan.bindings.clone();
                *self.reload_bindings.borrow_mut() = Some(views);
                let pins = self
                    .objects
                    .borrow()
                    .values()
                    .filter(|object| object.phase.get() == Phase::Live)
                    .filter(|object| plan.bindings.affected.contains(&object.id))
                    .map(|object| {
                        let contract = replays
                            .iter()
                            .find(|replay| Rc::ptr_eq(&replay.object, object))
                            .map(|replay| replay.candidate.service.contract.clone())
                            .unwrap_or_else(|| {
                                object
                                    .installed
                                    .borrow()
                                    .get(&object.current_epoch.get())
                                    .expect("live version")
                                    .service
                                    .contract
                                    .clone()
                            });
                        crate::Reference::pending(
                            &self,
                            object.clone(),
                            contract,
                            false,
                            next.version.get(),
                        )
                    })
                    .collect::<Vec<_>>();
                self.reload_ready.set(true);
                Ok(PreparedReload {
                    data: Some(ReadyData {
                        host: self,
                        next,
                        signal,
                        batch,
                        replays,
                        gate,
                        plan,
                        pins,
                        control,
                    }),
                    decided: false,
                    adopted: false,
                })
            }
        }
    }

    pub(super) async fn reload_wait<T>(
        &self,
        wait: LocalBoxFuture<'static, Result<T>>,
        timeout: Duration,
        signal: &CancellationToken,
        reason: &'static str,
    ) -> Result<T> {
        let stopped = futures::future::select(signal.cancelled(), self.driver.sleep(timeout))
            .map(move |result| match result {
                futures::future::Either::Left((error, _)) => error,
                futures::future::Either::Right(_) => {
                    RuntimeError::unavailable(reason, "reload deadline elapsed")
                }
            })
            .boxed_local();
        match futures::future::select(wait, stopped).await {
            futures::future::Either::Left((result, _)) => result,
            futures::future::Either::Right((error, _)) => Err(error),
        }
    }
}

impl PreparedReload {
    pub(crate) fn code_version(&self) -> u64 {
        self.data
            .as_ref()
            .expect("prepared owner")
            .next
            .version
            .get()
    }
    pub(crate) fn summary_counts(&self) -> (usize, usize) {
        let data = self.data.as_ref().expect("prepared owner");
        (
            data.replays.len(),
            data.replays
                .iter()
                .filter(|replay| {
                    replay.candidate.graph.requires_gate()
                        || replay.candidate.resources.requires_gate()
                })
                .count(),
        )
    }
    pub(crate) fn preflight(&mut self) -> Result<()> {
        let data = self
            .data
            .as_mut()
            .ok_or_else(|| RuntimeError::invalid("reload_state", "reload already ended"))?;
        if self.adopted {
            return Err(RuntimeError::invalid(
                "reload_state",
                "reload already adopted",
            ));
        }
        if !self.decided {
            data.signal.check()?;
        }
        data.host
            .preflight_commit(&data.batch, &mut data.plan, &data.replays, &data.next)
    }
    pub(crate) fn accept_parent_decision(&mut self) -> Result<()> {
        if self.data.is_none() || self.adopted || self.decided {
            return Err(RuntimeError::invalid(
                "reload_state",
                "parent decision requires an owned undecided Ready candidate",
            ));
        }
        self.decided = true;
        Ok(())
    }
    pub(crate) fn decide(&mut self) -> Result<()> {
        self.preflight()?;
        self.decided = true;
        Ok(())
    }
    pub(crate) fn adopt(&mut self) -> Result<ReloadReport> {
        if !self.decided {
            return Err(RuntimeError::invalid(
                "reload_state",
                "reload has no commit decision",
            ));
        }
        if let Err(error) = self.preflight() {
            return Err(self
                .data
                .as_ref()
                .expect("prepared owner")
                .host
                .fail_reload(error));
        }
        let data = self.data.as_mut().expect("prepared owner");
        let report = data
            .host
            .adopt_commit(
                &mut data.batch,
                &data.replays,
                &mut data.plan,
                &data.next,
                &mut data.gate,
            )
            .map_err(|cause| data.host.fail_reload(cause))?;
        self.adopted = true;
        data.host.release_reload_ready();
        if let Some(gate) = data.gate.take() {
            gate.open();
        }
        Ok(report)
    }
    pub(crate) async fn finish(mut self, mut report: ReloadReport) -> Result<ReloadReport> {
        if !self.adopted {
            return Err(RuntimeError::invalid(
                "reload_state",
                "reload has not adopted",
            ));
        }
        let data = self.data.take().expect("prepared owner");
        report.previous = data.host.epochs.previous().map(|epoch| epoch.version.get());
        report.lingering = data
            .host
            .objects
            .borrow()
            .values()
            .map(|object| {
                object
                    .active_contexts
                    .borrow()
                    .values()
                    .filter(|entry| {
                        entry.version < data.next.version.get()
                            && data.host.epochs.exact(entry.version).is_err()
                            && entry.cancellation.is_cancelled()
                    })
                    .count()
            })
            .sum();
        data.host.finish_reload_control(&data.control);
        drop(data.pins);
        Ok(report)
    }
    pub(crate) async fn abort(mut self, cause: RuntimeError) -> Result<()> {
        let Some(data) = self.data.take() else {
            return Ok(());
        };
        if self.decided {
            let error = data.host.fail_reload(cause);
            data.host.finish_reload_control(&data.control);
            return Err(error);
        }
        cleanup_ready(data, cause).await
    }
}
impl Drop for PreparedReload {
    fn drop(&mut self) {
        let Some(data) = self.data.take() else {
            return;
        };
        let decided = self.decided;
        let adopted = self.adopted;
        let driver = data.host.driver.clone();
        driver.spawn(
            async move {
                if adopted {
                    data.host.finish_reload_control(&data.control);
                } else {
                    let cause = RuntimeError::aborted(
                        "reload_abandoned",
                        "prepared reload administration ended",
                    );
                    if decided {
                        data.host.fail_reload(cause.clone());
                    }
                    let _ = cleanup_ready(data, cause).await;
                }
            }
            .boxed_local(),
        );
    }
}
async fn cleanup_ready(mut data: ReadyData, cause: RuntimeError) -> Result<()> {
    data.signal.cancel(cause.clone());
    data.batch.abort(cause.clone());
    // Release graph guards/barriers before invoking any asynchronous close or detach.
    data.plan.graphs.clear();
    drop(data.plan.barrier.take());
    data.plan.prepared.clear();
    data.host.release_reload_ready();
    let results = futures::future::join_all(
        data.replays
            .iter()
            .map(|replay| replay.candidate.resources.abort(cause.clone())),
    )
    .await;
    for replay in &data.replays {
        replay.candidate.graph.abort().finish();
        replay.refuse_fresh(&cause);
        for reference in replay.candidate.dependencies.borrow().values() {
            if !replay
                .prior
                .declarations
                .dependencies
                .values()
                .any(|prior| prior.same_claim(reference))
            {
                reference.release();
            }
        }
    }
    data.host.release_reload_ready();
    drop(data.gate.take());
    for replay in &data.replays {
        if replay.candidate.resources.installed_available()
            && let Some(runs) = replay.prior.declarations.activity_runs.borrow().clone()
        {
            runs.resume_after_abort().start();
        }
    }
    let abort = data.host.epochs.abort_pending(&data.next);
    data.host.finish_reload_control(&data.control);
    for error in results.into_iter().filter_map(Result::err) {
        data.host.errors.borrow_mut().push(error);
    }
    abort
}
impl HostInner {
    pub(crate) fn fail_reload(&self, cause: RuntimeError) -> RuntimeError {
        let failure = RuntimeError::unavailable(
            "host_failed",
            "Host cannot serve after a decided reload failed",
        )
        .with_details(serde_json::json!({"cause": cause}));
        self.epochs.fail(failure.clone());
        let current = self.epochs.current();
        current.cancellation.cancel(failure.clone());
        if let Some(previous) = self.epochs.previous() {
            previous.cancellation.cancel(failure.clone());
        }
        self.errors.borrow_mut().push(failure.clone());
        failure
    }
}
