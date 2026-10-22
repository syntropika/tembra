//! Fallible factory replay, candidate initialization and owned Ready plans.
use super::transaction::{CommitPlan, PendingActivities, Replay, claim_retires};
use super::*;
use crate::{
    Declaration, ErrorCode, Implementation, Result, RuntimeError,
    epoch::EpochRecord,
    host::{FactoryPass, HostInner, facets::PreparedFacetBatch},
    object::{FactoryScope, Phase},
    resource::ResourceReload,
    state::CandidateSources,
};
use futures::FutureExt;
use std::{cell::RefCell, rc::Rc};

impl HostInner {
    pub(super) fn ready_bindings(
        &self,
        prepared: &[(Rc<InstalledVersion>, PendingActivities)],
        removed_objects: &[u64],
        replays: &[Replay],
        version: u64,
    ) -> super::ReadyBindings {
        let mut objects = self.objects.borrow().values().cloned().collect::<Vec<_>>();
        objects.extend(
            replays
                .iter()
                .filter(|replay| replay.fresh)
                .map(|replay| replay.object.clone()),
        );
        let mut views = std::collections::HashMap::new();
        for object in objects {
            let installed = replays
                .iter()
                .position(|replay| Rc::ptr_eq(&replay.object, &object))
                .map(|position| prepared[position].0.clone())
                .or_else(|| {
                    object
                        .installed
                        .borrow()
                        .get(&object.current_epoch.get())
                        .cloned()
                });
            let Some(installed) = installed else {
                continue;
            };
            views.insert(
                (
                    object.id,
                    installed.service.contract().fingerprint().to_owned(),
                ),
                installed.implementation.clone(),
            );
            views.extend(
                installed
                    .adapters
                    .borrow()
                    .iter()
                    .map(|(fingerprint, view)| ((object.id, fingerprint.clone()), view.clone())),
            );
            for claim in object
                .claims
                .borrow()
                .iter()
                .filter_map(std::rc::Weak::upgrade)
            {
                if !claim_retires(replays, &claim)
                    && claim.contract.version() == installed.service.contract().version()
                {
                    views.insert(
                        (object.id, claim.contract.fingerprint().to_owned()),
                        installed.implementation.clone(),
                    );
                }
            }
        }
        super::ReadyBindings {
            version,
            views,
            blocked: removed_objects.iter().copied().collect(),
            affected: replays
                .iter()
                .map(|replay| replay.object.id)
                .chain(removed_objects.iter().copied())
                .collect(),
        }
    }
    pub(super) async fn prepare_replays(
        self: &Rc<Self>,
        batch: &PreparedFacetBatch,
        next: &Rc<EpochRecord>,
        signal: &CancellationToken,
        catalog: &Rc<CandidateCatalog>,
        replays: &mut Vec<Replay>,
    ) -> Result<()> {
        for service in batch
            .services
            .values()
            .filter(|service| service.options().lifetime == crate::Lifetime::Host)
        {
            let reference = catalog.hold(
                service.contract().clone(),
                serde_json::json!({}),
                crate::GetOptions {
                    state: false,
                    ..Default::default()
                },
            )?;
            reference.release();
        }
        loop {
            signal.check()?;
            let mut objects = self
                .objects
                .borrow()
                .values()
                .filter(|object| {
                    batch.services.contains_key(&object.key.0)
                        && matches!(object.phase.get(), Phase::Live | Phase::Activating)
                })
                .cloned()
                .collect::<Vec<_>>();
            objects.extend(catalog.pending.borrow().values().cloned());
            objects.retain(|object| {
                !replays
                    .iter()
                    .any(|replay| Rc::ptr_eq(&replay.object, object))
            });
            if objects.is_empty() {
                break;
            }
            for object in objects {
                let fresh = catalog
                    .pending
                    .borrow()
                    .values()
                    .any(|pending| Rc::ptr_eq(pending, &object));
                let service = if let Some(service) = batch.services.get(&object.key.0) {
                    service.clone()
                } else {
                    object
                        .activation_service
                        .borrow()
                        .as_ref()
                        .expect("private object service")
                        .as_ref()
                        .clone()
                };
                if !fresh && object.phase.get() == Phase::Activating {
                    let reference =
                        crate::Reference::new(self, object.clone(), object.contract.clone(), false);
                    let ready = reference.ready().await;
                    reference.release();
                    if ready.is_err() {
                        signal.check()?;
                        continue;
                    }
                }
                let prior = object
                    .installed
                    .borrow()
                    .get(&object.current_epoch.get())
                    .cloned();
                let prior = match prior {
                    Some(prior) => prior,
                    None if fresh => Rc::new(InstalledVersion {
                        epoch: self.epochs.current(),
                        service: Rc::new(service.clone()),
                        implementation: Rc::new(Implementation::new()),
                        declarations: Rc::new(DeclarationSet {
                            dependencies: Default::default(),
                            resources: crate::resource::ResourceSet::new(
                                Rc::downgrade(self),
                                Vec::new(),
                                Vec::new(),
                            ),
                            state: object.state.declarations(),
                            activities: crate::activity::ActivityDeclarations::new(),
                            activity_runs: RefCell::new(None),
                            registrations: Vec::new(),
                        }),
                        adapters: RefCell::default(),
                    }),
                    None => return Err(crate::epoch::superseded()),
                };
                let expected = service
                    .contract()
                    .normalize_input(&object.input)
                    .map_err(crate::host::contract_error)?;
                if service
                    .contract()
                    .identity(&expected)
                    .map_err(crate::host::contract_error)?
                    != object.key.1
                {
                    return Err(RuntimeError::new(
                        ErrorCode::ContractMismatch,
                        "identity",
                        "reload changes object identity",
                    ));
                }
                let pass_slot = Rc::new(RefCell::new(std::rc::Weak::<FactoryScope>::new()));
                let read_slot = pass_slot.clone();
                let host_weak = Rc::downgrade(self);
                let read = Rc::new(move || {
                    let host = host_weak
                        .upgrade()
                        .ok_or_else(|| RuntimeError::unavailable("closed", "Host ended"))?;
                    let weak_pass = read_slot.borrow().clone();
                    if weak_pass.upgrade().is_none()
                        || !std::rc::Weak::ptr_eq(&host.factory_pass.borrow(), &weak_pass)
                    {
                        return Err(RuntimeError::invalid(
                            "declaration_scope",
                            "candidate read is outside its factory pass",
                        ));
                    }
                    Ok(())
                });
                let candidate = Rc::new(CandidateDeclarations {
                    owner: Rc::downgrade(&object),
                    catalog: catalog.clone(),
                    service: Rc::new(service.clone()),
                    graph: object.state.begin_reload(read)?,
                    resources: ResourceReload::new_with_ready(&prior.declarations.resources, {
                        let catalog = Rc::downgrade(catalog);
                        Rc::new(move |reference| {
                            let catalog = catalog.upgrade();
                            async move {
                                catalog
                                    .ok_or_else(|| {
                                        RuntimeError::invalid(
                                            "reload_state",
                                            "candidate catalog ended",
                                        )
                                    })?
                                    .ready(reference)
                                    .await
                            }
                            .boxed_local()
                        })
                    }),
                    activities: crate::activity::ActivityDeclarations::new(),
                    dependencies: RefCell::default(),
                    registrations: RefCell::default(),
                });
                catalog.register(&object, &candidate);
                let pass = Rc::new(FactoryScope {
                    target: DeclarationTarget::Reload(Rc::downgrade(&candidate)),
                    implementation_owners: RefCell::new(Vec::new()),
                });
                *pass_slot.borrow_mut() = Rc::downgrade(&pass);
                replays.push(Replay {
                    object: object.clone(),
                    fresh,
                    prior: prior.clone(),
                    candidate: candidate.clone(),
                    implementation: Rc::new(Implementation::new()),
                    activity_failure: prior
                        .declarations
                        .activity_runs
                        .borrow()
                        .as_ref()
                        .map(|runs| runs.failure_checkpoint()),
                });
                let previous = self.factory_pass.replace(Rc::downgrade(&pass));
                self.in_factory.set(self.in_factory.get() + 1);
                let implementation = {
                    let _guard = FactoryPass {
                        host: self,
                        _pass: pass.clone(),
                        previous,
                    };
                    let mut declaration = Declaration {
                        host: self.clone(),
                        object: object.clone(),
                        pass: Rc::downgrade(&pass),
                    };
                    (service.factory)(expected, object.principal.clone(), &mut declaration)
                        .map(|implementation| pass.attach_implementation_owners(implementation))
                }?;
                Self::validate_implementation(service.contract(), &implementation)?;
                let mut implementation = implementation;
                let members = service
                    .contract()
                    .members(implementation.variant.as_deref().unwrap_or(""))
                    .expect("validated variant");
                for (name, source) in &mut implementation.states {
                    if !object.state.owns(source) {
                        *source = candidate.graph.forward(source.clone())?;
                    }
                    let tembra_contracts::Member::State { schema } = &members[name] else {
                        unreachable!()
                    };
                    let _ = schema;
                    candidate.graph.expose_definition(
                        source,
                        service.state_definition(
                            implementation.variant.as_deref().unwrap_or(""),
                            name,
                        ),
                    )?;
                }
                candidate.resources.preflight_adoption()?;
                replays.last_mut().expect("recorded replay").implementation =
                    Rc::new(implementation);
                catalog.recorded(
                    object.id,
                    &replays.last().expect("recorded replay").implementation,
                );
                signal.check()?;
            }
            if catalog.pending.borrow().values().any(|object| {
                !replays
                    .iter()
                    .any(|replay| Rc::ptr_eq(&replay.object, object))
            }) {
                continue;
            }
            let results = futures::future::join_all(replays.iter().map(|replay| {
                let reference = crate::Reference::new(
                    self,
                    replay.object.clone(),
                    replay.candidate.service.contract().clone(),
                    false,
                );
                reference.claim.object_owned.set(true);
                let ready = catalog.ready(reference.clone());
                async move {
                    let result = ready.await;
                    reference.release();
                    result
                }
            }))
            .await;
            results.into_iter().collect::<Result<Vec<_>>>()?;
        }
        self.epochs.preflight_next(next)
    }

    pub(super) async fn initialize_replays(
        self: &Rc<Self>,
        replays: &[Replay],
        sources: &Rc<CandidateSources>,
        signal: &CancellationToken,
    ) -> Result<()> {
        let failure = Rc::new(RefCell::new(None));
        let initializers = replays
            .iter()
            .flat_map(|replay| {
                replay
                    .candidate
                    .graph
                    .take_initializers()
                    .into_iter()
                    .map(move |plan| (replay, plan))
            })
            .collect::<Vec<_>>();
        futures::future::join_all(initializers.into_iter().map(|(replay, plan)| {
            let failure = failure.clone();
            let token = signal.clone();
            async move {
                let result = async {
                    let (context, _guard) = self.new_root(self.principal.clone());
                    token.link_child(context.cancellation());
                    context.set_attachment(Rc::new(super::super::InitializingObject(
                        Rc::downgrade(&replay.object),
                    )));
                    let context = sources.authorize_initializer(&context, &replay.candidate.graph);
                    let value = (plan.initialize)(context).await?;
                    token.check()?;
                    replay.candidate.graph.initialize(vec![(plan.cell, value)])
                }
                .await;
                if let Err(error) = result {
                    if failure.borrow().is_none() {
                        *failure.borrow_mut() = Some(error.clone());
                    }
                    token.cancel(error);
                }
            }
        }))
        .await;
        let error = failure.borrow().clone();
        error.map_or(Ok(()), Err)
    }

    pub(super) fn prepare_commit(
        self: &Rc<Self>,
        batch: &PreparedFacetBatch,
        replays: &[Replay],
        sources: &Rc<CandidateSources>,
        next: &Rc<EpochRecord>,
    ) -> Result<CommitPlan> {
        self.check_reload_service_ownership(batch)?;
        let removed_services = self.reload_removed_services(batch);
        let objects = self.objects.borrow().values().cloned().collect::<Vec<_>>();
        let mut removed_objects = objects
            .iter()
            .filter(|object| removed_services.contains(&object.key.0))
            .map(|object| object.id)
            .collect::<Vec<_>>();
        // A dependent owner must close before the service its closer can still use.
        loop {
            let next = objects
                .iter()
                .filter(|object| !removed_objects.contains(&object.id))
                .filter(|object| {
                    !replays
                        .iter()
                        .any(|replay| Rc::ptr_eq(&replay.object, object))
                        && object.installed.borrow().values().any(|installed| {
                            installed
                                .declarations
                                .dependencies
                                .values()
                                .any(|reference| removed_objects.contains(&reference.object_id()))
                        })
                })
                .map(|object| object.id)
                .collect::<Vec<_>>();
            if next.is_empty() {
                break;
            }
            removed_objects.extend(next);
        }
        let mut replacements = Vec::new();
        let mut prepared = Vec::new();
        for replay in replays {
            let mut installed = Rc::new(InstalledVersion {
                epoch: next.clone(),
                service: replay.candidate.service.clone(),
                implementation: replay.implementation.clone(),
                declarations: Rc::new(DeclarationSet {
                    dependencies: replay.candidate.dependencies.borrow().clone(),
                    resources: replay.candidate.resources.declarations(),
                    state: replay.candidate.graph.declarations(),
                    activities: replay.candidate.activities.clone(),
                    activity_runs: RefCell::new(None),
                    registrations: replay.candidate.registrations.borrow().clone(),
                }),
                adapters: RefCell::new(replay.candidate.catalog.adapter_views(replay.object.id)),
            });
            let consumers = replay
                .object
                .claims
                .borrow()
                .iter()
                .filter_map(std::rc::Weak::upgrade)
                .filter(|claim| !claim_retires(replays, claim))
                .filter(|claim| {
                    !matches!(
                        (crate::Reference {
                            claim: claim.clone()
                        })
                        .status(),
                        crate::RefStatus::Released | crate::RefStatus::Gone
                    )
                })
                .map(|claim| claim.contract.clone())
                .collect::<Vec<_>>();
            for consumer in consumers {
                installed.service.bind(&consumer)?;
                if consumer.version() == installed.service.contract().version() {
                    continue;
                }
                if installed
                    .adapters
                    .borrow()
                    .contains_key(consumer.fingerprint())
                {
                    continue;
                }
                let adapter = installed
                    .service
                    .options()
                    .accepts
                    .get(&consumer.version())
                    .expect("binding checked adapter");
                let mut view = adapter(installed.implementation.clone())?;
                Self::validate_view(&consumer, &view, false)?;
                let members = consumer
                    .members(view.variant.as_deref().unwrap_or(""))
                    .expect("checked adapter variant");
                for (name, member) in members {
                    let tembra_contracts::Member::State { schema } = member else {
                        continue;
                    };
                    let source = view.states.get_mut(name).expect("validated consumer state");
                    if !replay.object.state.owns(source) {
                        *source = replay.candidate.graph.forward(source.clone())?;
                    }
                    replay.candidate.graph.expose(source, schema.clone())?;
                }
                installed
                    .adapters
                    .borrow_mut()
                    .insert(consumer.fingerprint().into(), Rc::new(view));
            }
            let private = Rc::get_mut(&mut installed)
                .expect("candidate version has not escaped into activity observers");
            Rc::get_mut(&mut private.declarations)
                .expect("private declaration journal")
                .state = replay.candidate.graph.declarations();
            let previous = replay.prior.declarations.activity_runs.borrow().clone();
            let activities = replay.candidate.activities.prepare(
                self,
                &replay.object,
                installed.clone(),
                previous.as_ref(),
            )?;
            replacements.push((
                replay.object.clone(),
                replay.candidate.service.contract().clone(),
                replay.candidate.registrations.borrow().clone(),
            ));
            prepared.push((installed, PendingActivities(Some(activities))));
        }
        let mut bindings =
            self.ready_bindings(&prepared, &removed_objects, replays, next.version.get());
        let proxy_links = self
            .objects
            .borrow()
            .values()
            .flat_map(|object| {
                object
                    .claims
                    .borrow()
                    .iter()
                    .filter_map(std::rc::Weak::upgrade)
                    .filter(|claim| !claim.binding_ended() && !claim_retires(replays, claim))
                    .flat_map(|claim| {
                        bindings
                            .views
                            .get(&(object.id, claim.contract.fingerprint().to_owned()))
                            .map(|view| claim.projection_links(view))
                            .unwrap_or_default()
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        let installed_graphs = self
            .objects
            .borrow()
            .values()
            .filter(|object| object.phase.get() == Phase::Live)
            .map(|object| {
                object
                    .installed
                    .borrow()
                    .get(&object.current_epoch.get())
                    .expect("live object has its current declaration set")
                    .declarations
                    .state
                    .clone()
            })
            .collect();
        let sources = sources.with_dependents(installed_graphs, proxy_links)?;
        let barrier = sources.ready_barrier()?;
        let mut graphs = sources.prepare_all()?;
        bindings.affected.extend(
            self.objects
                .borrow()
                .values()
                .filter(|object| {
                    graphs
                        .iter()
                        .any(|graph| Rc::ptr_eq(graph.graph(), &object.state))
                })
                .map(|object| object.id),
        );
        let tables =
            self.registries
                .prepare_epoch(next.clone(), &replacements, &removed_objects)?;
        for graph in &graphs {
            graph.preflight()?;
        }
        for replay in replays {
            replay.candidate.resources.preflight()?;
        }
        for (_, activities) in &mut prepared {
            activities.get_mut().preflight()?;
        }
        self.registries.preflight_epoch(&tables)?;
        self.epochs.preflight_next(next)?;
        for graph in &mut graphs {
            graph.release_preparing();
        }
        let mut plan = CommitPlan {
            bindings,
            graphs,
            tables,
            prepared,
            removed_objects,
            barrier: Some(barrier),
        };
        self.preflight_commit(batch, &mut plan, replays, next)?;
        Ok(plan)
    }

    pub(super) fn preflight_commit(
        &self,
        batch: &PreparedFacetBatch,
        plan: &mut CommitPlan,
        replays: &[Replay],
        next: &Rc<EpochRecord>,
    ) -> Result<()> {
        self.check_reload_service_ownership(batch)?;
        self.epochs.preflight_next(next)?;
        for object in self.objects.borrow().values() {
            if plan.removed_objects.contains(&object.id)
                || !plan.bindings.affected.contains(&object.id)
            {
                continue;
            }
            for claim in object
                .claims
                .borrow()
                .iter()
                .filter_map(std::rc::Weak::upgrade)
            {
                if !claim.binding_ended()
                    && !claim_retires(replays, &claim)
                    && !plan
                        .bindings
                        .views
                        .contains_key(&(object.id, claim.contract.fingerprint().to_owned()))
                {
                    return Err(RuntimeError::new(
                        ErrorCode::UpgradeConflict,
                        "upgrade_conflict",
                        "consumer joined after pending binding validation",
                    ));
                }
            }
        }
        for replay in replays {
            if replay.fresh {
                if self
                    .objects
                    .borrow()
                    .get(&replay.object.key)
                    .is_some_and(|other| !Rc::ptr_eq(other, &replay.object))
                {
                    return Err(RuntimeError::new(
                        ErrorCode::UpgradeConflict,
                        "upgrade_conflict",
                        "candidate address acquired a concurrent installed object",
                    ));
                }
            } else if replay.object.phase.get() != Phase::Live {
                return Err(RuntimeError::new(
                    ErrorCode::UpgradeConflict,
                    "upgrade_conflict",
                    "candidate owner ended before commit",
                ));
            }
        }
        self.registries.preflight_epoch(&plan.tables)?;
        for graph in &plan.graphs {
            graph.preflight()?;
        }
        for replay in replays {
            replay.candidate.resources.preflight()?;
        }
        for (_, activities) in &mut plan.prepared {
            activities.get_mut().preflight()?;
        }
        Ok(())
    }
}
