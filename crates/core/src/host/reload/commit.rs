//! Coherent adoption of already checked local domains, followed by publication.
use super::transaction::{CommitPlan, Replay, claim_retires};
use super::*;
use crate::{
    ErrorCode, Result, RuntimeError,
    activity::ActivityActions,
    epoch::EpochRecord,
    host::{HostInner, facets::PreparedFacetBatch},
    object::Phase,
};
use futures::FutureExt;
use std::{cell::RefCell, rc::Rc};

impl HostInner {
    pub(super) fn adopt_commit(
        self: &Rc<Self>,
        batch: &mut PreparedFacetBatch,
        replays: &[Replay],
        plan: &mut CommitPlan,
        next: &Rc<EpochRecord>,
        gate: &mut Option<crate::rpc::roots::RootGate>,
    ) -> Result<ReloadReport> {
        self.preflight_commit(batch, plan, replays, next)?;
        let CommitPlan {
            graphs,
            tables,
            prepared,
            removed_objects,
            bindings,
            ..
        } = plan;
        let hold = self.publication_queue.hold();
        let mut retired_resources = Vec::new();
        let mut actions = ActivityActions::default();
        for (replay, (installed, activities)) in replays.iter().zip(prepared.iter_mut()) {
            let (resources, retired) = replay.candidate.resources.adopt();
            retired_resources.push(retired);
            let _ = resources; // Same records are already owned by the private declaration preview.
            let (runs, next_actions) = activities.get_mut().adopt();
            let finished_activities = activities.0.take();
            drop(finished_activities);
            *installed.declarations.activity_runs.borrow_mut() = Some(runs);
            actions.extend(next_actions);
        }
        for graph in graphs.iter_mut() {
            let events = graph.adopt();
            self.publication_queue.append_batch(events);
        }
        let transition = self.epochs.adopt_next(next);
        for replay in replays {
            for reference in replay.prior.declarations.dependencies.values() {
                if claim_retires(replays, &reference.claim) {
                    reference.claim.freeze_binding(replay.prior.version().get());
                }
            }
            for reference in replay.candidate.dependencies.borrow().values() {
                reference.claim.publish_binding();
            }
        }
        if let Some(gate) = gate.as_ref() {
            gate.adopt_minimum(next.version.get());
        }
        let retired_facets = self.adopt_reload_facets(batch);
        let mut retired_factories = Vec::new();
        for replay in replays.iter().filter(|replay| replay.fresh) {
            self.objects
                .borrow_mut()
                .insert(replay.object.key.clone(), replay.object.clone());
            replay.object.phase.set(Phase::Live);
            // The selected service remains authoritative; dead address handles need no factory.
            let factory = replay.object.factory.borrow_mut().take();
            let service = replay.object.activation_service.borrow_mut().take();
            retired_factories.push((factory, service));
        }
        let all = self
            .objects
            .borrow()
            .values()
            .filter(|object| object.phase.get() == Phase::Live)
            .cloned()
            .collect::<Vec<_>>();
        for object in &all {
            let replacement = replays
                .iter()
                .position(|replay| Rc::ptr_eq(&replay.object, object));
            let installed = if let Some(index) = replacement {
                prepared[index].0.clone()
            } else {
                let prior = object
                    .installed
                    .borrow()
                    .get(&object.current_epoch.get())
                    .cloned()
                    .expect("live installed version");
                Rc::new(InstalledVersion {
                    epoch: transition.current.clone(),
                    service: prior.service.clone(),
                    implementation: prior.implementation.clone(),
                    declarations: prior.declarations.clone(),
                    adapters: RefCell::new(prior.adapters.borrow().clone()),
                })
            };
            object
                .installed
                .borrow_mut()
                .insert(next.version.get(), installed.clone());
            object.current_epoch.set(next.version.get());
            let retired_dependencies = object
                .dependencies
                .replace(installed.declarations.dependencies.clone());
            let retired_resources = object
                .resources
                .replace(installed.declarations.resources.records().to_vec());
            let retired_opened = object
                .opened
                .replace(installed.declarations.resources.opened().to_vec());
            let retired_registrations = object
                .registrations
                .replace(installed.declarations.registrations.clone());
            drop((
                retired_dependencies,
                retired_resources,
                retired_opened,
                retired_registrations,
            ));
            let old_impl = object
                .implementation
                .replace(Some(installed.implementation.clone()));
            drop(old_impl); // Also retained by the preceding installed version.
        }
        let publications = self.registries.adopt_epoch(tables);
        self.publication_queue.append_registry_batch(publications);
        let claims = all
            .iter()
            .flat_map(|object| {
                object
                    .claims
                    .borrow()
                    .iter()
                    .filter_map(std::rc::Weak::upgrade)
                    .filter(|claim| !claim.binding_ended() && !claim_retires(replays, claim))
                    .map(|claim| (object.id, claim))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let mut snapshots = Vec::new();
        let mut binding_errors = Vec::new();
        for (id, claim) in &claims {
            if bindings.affected.contains(id)
                && (!removed_objects.contains(id) || claim.object_owned.get())
            {
                let view = bindings
                    .views
                    .get(&(*id, claim.contract.fingerprint().to_owned()))
                    .expect("preflight retains every selected consumer view");
                match claim.reload_proxy_snapshots(view) {
                    Ok(values) => snapshots.extend(values),
                    Err(error) => binding_errors.push((claim.clone(), error)),
                }
            }
        }
        // Only existing hydrated ports are staged silently. Normal binding refresh below
        // emits their single Reset after every peer already exposes the coherent values.
        for (port, snapshot) in snapshots {
            let staged = port.adopt(crate::StateEvent::Reset(snapshot));
            drop(staged);
        }
        for (_, claim) in &claims {
            claim.publish_binding();
            claim.begin_reload_publication();
        }
        // Disable every admission barrier before the first detach, wake or publication.
        let ready_changed = self.take_reload_ready_notification();
        if let Some(gate) = gate.take() {
            gate.open_silent();
        }
        drop(plan.barrier.take());
        self.roots.notify_change();
        if let Some(changed) = ready_changed {
            changed.cancel(RuntimeError::aborted(
                "reload_ready",
                "coherent reload adopted",
            ));
        }
        for graph in std::mem::take(graphs) {
            graph.finish();
        }
        for (claim, error) in &binding_errors {
            claim.mark_gone(error.clone());
        }
        for (id, claim) in &claims {
            if !removed_objects.contains(id) || claim.object_owned.get() {
                if binding_errors
                    .iter()
                    .any(|(failed, _)| Rc::ptr_eq(failed, claim))
                {
                    continue;
                }
                claim.refresh_local_binding();
            }
        }
        for (id, claim) in &claims {
            if removed_objects.contains(id) && !claim.object_owned.get() {
                claim.mark_gone(RuntimeError::new(
                    ErrorCode::Gone,
                    "service_removed",
                    "service or managed dependency was removed",
                ));
            }
        }
        for (_, claim) in &claims {
            claim.finish_reload_publication();
        }
        for replay in replays.iter().filter(|replay| replay.fresh) {
            replay.object.finish_ready(Ok(()));
        }
        drop(hold);

        for retired in retired_resources {
            retired.finish();
        }
        retired_facets.finish();
        drop(retired_factories);
        actions.start();
        let removed = all
            .iter()
            .filter(|object| removed_objects.contains(&object.id))
            .cloned()
            .collect::<Vec<_>>();
        if !removed.is_empty() {
            for object in &removed {
                object.host_pin.set(false);
            }
            let done = CancellationToken::new();
            self.retirements.borrow_mut().push(done.clone());
            let host = self.clone();
            self.driver.spawn(
                async move {
                    if let Err(error) = host
                        .dispose_bulk(
                            removed,
                            RuntimeError::new(
                                ErrorCode::Gone,
                                "service_removed",
                                "service or managed dependency was removed",
                            ),
                        )
                        .await
                    {
                        host.errors.borrow_mut().push(error);
                    }
                    done.cancel(RuntimeError::aborted(
                        "retired",
                        "removed service cleanup settled",
                    ));
                }
                .boxed_local(),
            );
        }
        Ok(ReloadReport {
            current: transition.current.version.get(),
            previous: Some(transition.previous.version.get()),
            changed: batch.changed.clone(),
            removed: batch.removed.clone(),
            prepared: replays.iter().map(|replay| replay.object.id).collect(),
            gated: replays
                .iter()
                .filter(|replay| {
                    replay.candidate.graph.requires_gate()
                        || replay.candidate.resources.requires_gate()
                })
                .map(|replay| replay.object.id)
                .collect(),
            lingering: 0,
        })
    }
}
