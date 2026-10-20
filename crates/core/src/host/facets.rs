use super::*;
use crate::facet::{
    ExecutableFacet, FacetDeclaration, FacetScope, FacetSignal, FacetStartup, LoadOptions,
    NativeFacet, SetupPass, SetupScope,
};
use futures::{channel::oneshot, future::poll_fn};
use std::{any::Any, collections::BTreeMap, task::Poll};

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct FacetRunId(u64);
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct InstalledServiceId(pub(crate) u64);

pub(super) struct InstalledRun {
    id: FacetRunId,
    definition: ExecutableFacet,
    cancellation: CancellationToken,
    retained: RefCell<Option<Vec<Rc<dyn Any>>>>,
}
impl InstalledRun {
    fn prepare(id: FacetRunId, definition: impl Into<ExecutableFacet>) -> Rc<Self> {
        Rc::new(Self {
            id,
            definition: definition.into(),
            cancellation: CancellationToken::new(),
            retained: RefCell::new(Some(Vec::new())),
        })
    }
    pub(super) fn abort(&self, cause: RuntimeError) {
        // End admission before cancellation wakers can reenter. Neither waking nor user
        // capture destruction may observe a retained-owner storage borrow.
        let retained = self.retained.borrow_mut().take();
        self.cancellation.cancel(cause);
        drop(retained);
    }
}
struct PendingActivation {
    object: Rc<ObjectRecord>,
    previous: Option<Rc<ObjectRecord>>,
}
#[derive(Default)]
pub(super) struct FacetState {
    next_run: Cell<u64>,
    next_service: Cell<u64>,
    depth: Cell<usize>,
    setup_pass: RefCell<Weak<SetupScope>>,
    pending: RefCell<Vec<PendingActivation>>,
    runs: RefCell<HashMap<String, Rc<InstalledRun>>>,
    pub(super) startups: RefCell<BTreeMap<InstalledServiceId, Rc<HostActivation>>>,
}
impl FacetState {
    fn commit_run(&self, run: Rc<InstalledRun>) {
        let old = self
            .runs
            .borrow_mut()
            .insert(run.definition.name().to_owned(), run.clone());
        if let Some(old) = old
            && !Rc::ptr_eq(&old, &run)
        {
            old.abort(RuntimeError::new(
                ErrorCode::Gone,
                "facet_replaced",
                "facet replaced or removed",
            ));
        }
    }
    fn take_run(&self, name: &str, expected: FacetRunId) -> Option<Rc<InstalledRun>> {
        let mut runs = self.runs.borrow_mut();
        if runs.get(name).is_some_and(|run| run.id == expected) {
            runs.remove(name)
        } else {
            None
        }
    }
    pub(super) fn close_runs(&self) {
        let runs: Vec<_> = self.runs.borrow().values().cloned().collect();
        for run in runs {
            run.abort(RuntimeError::new(
                ErrorCode::Gone,
                "host_closed",
                "Host closed",
            ));
        }
    }
    pub(super) fn release_runs(&self) {
        let runs = std::mem::take(&mut *self.runs.borrow_mut());
        let startups = std::mem::take(&mut *self.startups.borrow_mut());
        drop((runs, startups));
    }
}

pub(crate) struct SetupJournal {
    host: Weak<HostInner>,
    run: FacetRunId,
    name: String,
    services: Vec<InstalledServiceId>,
    active: bool,
    candidate: Option<Rc<RefCell<HashMap<String, Service>>>>,
    candidate_run: Option<Rc<InstalledRun>>,
    candidate_replaced: Option<Rc<HashSet<FacetRunId>>>,
}
impl Drop for SetupJournal {
    fn drop(&mut self) {
        if self.active
            && let Some(host) = self.host.upgrade()
        {
            host.withdraw_setup(
                self,
                RuntimeError::aborted("setup_unwound", "facet setup unwound"),
            );
        }
    }
}

pub(crate) struct DeclarationBatch {
    host: Weak<HostInner>,
    active: bool,
}
impl DeclarationBatch {
    pub(crate) fn finish(mut self) {
        self.release();
    }
    fn release(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        if let Some(host) = self.host.upgrade() {
            host.facets.depth.set(host.facets.depth.get() - 1);
            if !std::thread::panicking() {
                host.flush_declarations();
            }
        }
    }
}
impl Drop for DeclarationBatch {
    fn drop(&mut self) {
        self.release();
    }
}

/// Retains only the eager activation result, independent of disposed object storage.
pub(crate) struct HostActivation {
    result: RefCell<Option<Result<()>>>,
    waiters: RefCell<HashMap<u64, oneshot::Sender<Result<()>>>>,
    next_waiter: Cell<u64>,
}
impl HostActivation {
    fn new() -> Rc<Self> {
        Rc::new(Self {
            result: RefCell::new(None),
            waiters: RefCell::new(HashMap::new()),
            next_waiter: Cell::new(0),
        })
    }
    fn settle(&self, result: Result<()>) -> Option<RuntimeError> {
        {
            let mut settled = self.result.borrow_mut();
            if settled.is_some() {
                return None;
            }
            *settled = Some(result.clone());
        }
        let waiters = std::mem::take(&mut *self.waiters.borrow_mut());
        for waiter in waiters.into_values() {
            let _ = waiter.send(result.clone());
        }
        result.err()
    }
    fn wait(self: &Rc<Self>) -> LocalBoxFuture<'static, Result<()>> {
        let activation = self.clone();
        async move {
            if let Some(result) = activation.result.borrow().clone() {
                return result;
            }
            let (send, receive) = oneshot::channel();
            let id = activation.next_waiter.get() + 1;
            activation.next_waiter.set(id);
            activation.waiters.borrow_mut().insert(id, send);
            let _guard = StartupWaiter {
                activation: Rc::downgrade(&activation),
                id,
            };
            receive.await.unwrap_or_else(|_| {
                Err(RuntimeError::unavailable(
                    "activation",
                    "startup readiness interrupted",
                ))
            })
        }
        .boxed_local()
    }
}
struct StartupWaiter {
    activation: Weak<HostActivation>,
    id: u64,
}
impl Drop for StartupWaiter {
    fn drop(&mut self) {
        if let Some(activation) = self.activation.upgrade() {
            let waiter = activation.waiters.borrow_mut().remove(&self.id);
            drop(waiter);
        }
    }
}
fn readiness(records: Vec<Rc<HostActivation>>) -> LocalBoxFuture<'static, Result<()>> {
    let mut waits: Vec<_> = records.iter().map(|record| Some(record.wait())).collect();
    poll_fn(move |cx| {
        let mut failure = None;
        let mut pending = false;
        for wait in &mut waits {
            let Some(future) = wait else {
                continue;
            };
            match future.as_mut().poll(cx) {
                Poll::Pending => pending = true,
                Poll::Ready(result) => {
                    if failure.is_none() {
                        failure = result.err();
                    }
                    *wait = None;
                }
            }
        }
        if let Some(error) = failure {
            Poll::Ready(Err(error))
        } else if pending {
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    })
    .boxed_local()
}

impl Host {
    /// Snapshots the actual installed facet catalog in case-sensitive Rust `str` order.
    ///
    /// Failed synchronous setup withdraws its scoped run; an eager activation failure does
    /// not remove an admitted run; [`FacetStartup`] observes readiness separately. Candidates
    /// remain absent through setup and activation until [`Host::reload`] commits, which may
    /// precede resolution of its waiter. During initial synchronous setup, a reentrant query
    /// can observe that setup's scoped run before its outcome. The catalog is empty after
    /// [`Host::close`] finishes, including a reported cleanup failure.
    ///
    /// Presence establishes catalog admission, not readiness, startup success or provenance.
    /// Absence does not establish that old calls stopped or resources were physically released.
    /// Owned values remain unchanged and retain no executable callbacks or resources. Equality
    /// compares only name and origin; same-name replacement can produce equal snapshots.
    ///
    /// ```
    /// use serde_json::json;
    /// use tembra_core::{FacetKind, Host, HostOptions, InstalledFacet, LoadOptions, ManualDriver, NativeFacet};
    /// let driver = ManualDriver::new();
    /// let host = Host::new(HostOptions::new("catalog", json!(null)), driver.clone());
    /// assert!(host.installed_facets().is_empty());
    /// let facet = NativeFacet::new("compiled", vec![], |_, _| Ok(()))?;
    /// driver.run_until(host.install(facet, LoadOptions::default())?)?;
    /// let snapshot: Vec<InstalledFacet> = host.installed_facets();
    /// assert_eq!(snapshot[0].name(), "compiled");
    /// assert_eq!(snapshot[0].kind(), FacetKind::Native);
    /// driver.run_until(host.close())?;
    /// assert!(host.installed_facets().is_empty());
    /// assert_eq!(snapshot[0].name(), "compiled"); // The prior value is an owned snapshot.
    /// # Ok::<(), tembra_core::RuntimeError>(())
    /// ```
    pub fn installed_facets(&self) -> Vec<crate::InstalledFacet> {
        let mut facets = self
            .inner
            .facets
            .runs
            .borrow()
            .values()
            .map(|run| crate::InstalledFacet::from_definition(&run.definition))
            .collect::<Vec<_>>();
        facets.sort_by(|left, right| left.name().cmp(right.name()));
        facets
    }

    /// Declares one executable native facet synchronously and returns its startup waiter.
    ///
    /// # Errors
    /// Refuses duplicate identity, closed Hosts or setup errors before returning. A failed
    /// setup withdraws its scoped services; external effects are not rolled back. The
    /// returned waiter preserves eager activation failures without removing installed work.
    /// A Ready barrier returns retryable [`ErrorCode::UpgradeConflict`] before setup runs.
    pub fn install(&self, facet: NativeFacet, options: LoadOptions) -> Result<FacetStartup> {
        self.load(vec![facet], options)
    }

    /// Declares the batch before any factory starts, then selects eager readiness.
    ///
    /// Dropping the returned waiter does not stop installed work. If a setup fails, earlier
    /// successful facets remain installed and start after the declaration barrier ends.
    ///
    /// # Errors
    /// Returns the original synchronous setup/admission failure, withdrawing only that
    /// facet's scoped declarations. Later facets are skipped; external effects remain.
    /// A Ready barrier returns retryable [`ErrorCode::UpgradeConflict`] before setup runs.
    pub fn load(&self, facets: Vec<NativeFacet>, options: LoadOptions) -> Result<FacetStartup> {
        self.load_executable(
            facets.into_iter().map(ExecutableFacet::from).collect(),
            options,
        )
    }

    /// Installs one native or foreign definition through the existing synchronous journal.
    ///
    /// # Errors
    /// Has the same declaration, duplicate and startup errors as [`Host::install`].
    /// Installation does not replace existing facets or roll back setup effects.
    pub fn install_executable(
        &self,
        facet: ExecutableFacet,
        options: LoadOptions,
    ) -> Result<FacetStartup> {
        self.load_executable(vec![facet], options)
    }

    /// Declares a mixed-origin batch before any object factory starts.
    ///
    /// # Errors
    /// Preserves [`Host::load`] failure/withdrawal semantics. Native definitions stay stable;
    /// duplicate names are refused regardless of origin.
    pub fn load_executable(
        &self,
        facets: Vec<ExecutableFacet>,
        options: LoadOptions,
    ) -> Result<FacetStartup> {
        let batch = self.inner.begin_declarations()?;
        let result = (|| {
            let mut selected = Vec::new();
            for facet in facets {
                let journal = Rc::new(RefCell::new(self.inner.begin_setup(facet)?));
                let name = journal.borrow().name.clone();
                let (definition, token) = {
                    let runs = self.inner.facets.runs.borrow();
                    let run = &runs[&name];
                    (run.definition.clone(), run.cancellation.clone())
                };
                let scope = FacetScope::new(&self.inner, FacetSignal::new(token));
                let setup = {
                    let (_pass, weak) = SetupPass::enter(&self.inner);
                    definition.setup(
                        &scope,
                        &mut FacetDeclaration::new(&self.inner, &journal, weak),
                    )
                };
                // Only weak recorders can escape setup. Extract before any cancellation/drop.
                let mut journal = match Rc::try_unwrap(journal) {
                    Ok(journal) => journal.into_inner(),
                    Err(_) => {
                        unreachable!("setup journal strong ownership never escapes installer")
                    }
                };
                if let Err(error) = setup {
                    self.inner.withdraw_setup(&mut journal, error.clone());
                    return Err(error);
                }
                selected.extend(self.inner.finish_setup(journal)?);
            }
            Ok(selected)
        })();
        batch.finish();
        let selected = result?;
        Ok(FacetStartup::new(if options.await_host_objects {
            readiness(selected)
        } else {
            async { Ok(()) }.boxed_local()
        }))
    }
}

impl HostInner {
    pub(crate) fn begin_declarations(self: &Rc<Self>) -> Result<DeclarationBatch> {
        self.epochs.check()?;
        if self.reload_ready.get() {
            return Err(RuntimeError::new(
                ErrorCode::UpgradeConflict,
                "upgrade_conflict",
                "reload commit barrier blocks installation",
            ));
        }

        if self.closing.get() {
            return Err(RuntimeError::unavailable("closed", "Host is closing"));
        }
        self.facets.depth.set(self.facets.depth.get() + 1);
        Ok(DeclarationBatch {
            host: Rc::downgrade(self),
            active: true,
        })
    }
    fn begin_setup(self: &Rc<Self>, facet: ExecutableFacet) -> Result<SetupJournal> {
        if self.facets.runs.borrow().contains_key(facet.name()) {
            return Err(RuntimeError::invalid(
                "duplicate_facet",
                "facet is already installed",
            ));
        }
        let next = checked_next(&self.facets.next_run)?;
        let id = FacetRunId(next);
        let name = facet.name().to_owned();
        self.facets.commit_run(InstalledRun::prepare(id, facet));
        Ok(SetupJournal {
            host: Rc::downgrade(self),
            run: id,
            name,
            services: Vec::new(),
            active: true,
            candidate: None,
            candidate_run: None,
            candidate_replaced: None,
        })
    }
    pub(crate) fn replace_setup_pass(&self, pass: Weak<SetupScope>) -> Weak<SetupScope> {
        self.facets.setup_pass.replace(pass)
    }
    pub(crate) fn retain_run_owner(
        self: &Rc<Self>,
        journal: &Rc<RefCell<SetupJournal>>,
        pass: &Weak<SetupScope>,
        owner: Rc<dyn Any>,
    ) -> Result<()> {
        let (id, name, candidate) = {
            let journal = journal.borrow();
            if pass.upgrade().is_none()
                || !Weak::ptr_eq(pass, &self.facets.setup_pass.borrow())
                || !journal.active
                || !Weak::ptr_eq(&journal.host, &Rc::downgrade(self))
            {
                return Err(RuntimeError::invalid(
                    "setup_scope",
                    "recorder is outside its exact setup pass",
                ));
            }
            (
                journal.run,
                journal.name.clone(),
                journal.candidate_run.clone(),
            )
        };
        if self.closing.get() {
            return Err(RuntimeError::unavailable("closed", "Host is closing"));
        }
        let run = candidate.or_else(|| self.facets.runs.borrow().get(&name).cloned());
        let run = run
            .filter(|run| run.id == id)
            .ok_or_else(|| RuntimeError::invalid("setup_scope", "run is no longer installed"))?;
        let mut retained = run.retained.borrow_mut();
        if let Some(retained) = retained.as_mut() {
            retained.push(owner);
            return Ok(());
        }
        drop(retained);
        Err(RuntimeError::invalid("setup_scope", "facet run is retired"))
    }
    pub(crate) fn serve_owned(
        self: &Rc<Self>,
        journal: &Rc<RefCell<SetupJournal>>,
        pass: &Weak<SetupScope>,
        contract: Rc<Contract>,
        factory: Factory,
        options: ServeOptions,
    ) -> Result<InstalledServiceId> {
        // Validate/copy authority only; rejected callback capture Drop must not see
        // a journal borrow.
        let run = {
            let journal = journal.borrow();
            if pass.upgrade().is_none()
                || !Weak::ptr_eq(pass, &self.facets.setup_pass.borrow())
                || !journal.active
                || !Weak::ptr_eq(&journal.host, &Rc::downgrade(self))
            {
                return Err(RuntimeError::invalid(
                    "setup_scope",
                    "declaration capability is outside its exact setup pass",
                ));
            }
            journal.run
        };
        // Successful insertion moves captures into storage and invokes no user code. Error
        // drops happen here, outside the journal borrow; there is no queued service on error.
        let candidate = journal.borrow().candidate.clone();
        let id = if let Some(candidate) = candidate {
            if candidate.borrow().contains_key(contract.name()) {
                return Err(RuntimeError::invalid(
                    "duplicate",
                    "candidate type is already served",
                ));
            }
            self.validate_service(&contract, &options)?;
            let replaced = journal
                .borrow()
                .candidate_replaced
                .clone()
                .expect("candidate setup has exact replaced owners");
            if self
                .services
                .borrow()
                .get(contract.name())
                .is_some_and(|service| {
                    !service.owner.is_some_and(|owner| replaced.contains(&owner))
                })
            {
                return Err(RuntimeError::invalid(
                    "duplicate",
                    "candidate type is owned by an unreplaced service",
                ));
            }
            let ordinal = self
                .services
                .borrow()
                .get(contract.name())
                .map(|service| service.ordinal)
                .unwrap_or(InstalledServiceId(checked_next(&self.facets.next_service)?));
            candidate.borrow_mut().insert(
                contract.name().into(),
                Service {
                    contract,
                    factory,
                    options,
                    ordinal,
                    owner: Some(run),
                },
            );
            ordinal
        } else {
            self.insert_service(contract, factory, options, Some(run))?
        };
        journal.borrow_mut().services.push(id);
        Ok(id)
    }
    fn finish_setup(self: &Rc<Self>, mut journal: SetupJournal) -> Result<Vec<Rc<HostActivation>>> {
        let mut records = Vec::new();
        for id in &journal.services {
            if let Some(record) = self.start_host_service(*id)? {
                records.push(record);
            }
        }
        journal.active = false;
        Ok(records)
    }
    fn withdraw_setup(&self, journal: &mut SetupJournal, cause: RuntimeError) {
        if !journal.active {
            return;
        }
        journal.active = false;
        if let Some(candidate) = journal.candidate.take() {
            let run = journal.candidate_run.take();
            let removed = {
                let mut services = candidate.borrow_mut();
                let names = services
                    .iter()
                    .filter(|(_, service)| service.owner == Some(journal.run))
                    .map(|(name, _)| name.clone())
                    .collect::<Vec<_>>();
                names
                    .into_iter()
                    .filter_map(|name| services.remove(&name))
                    .collect::<Vec<_>>()
            };
            if let Some(run) = &run {
                run.abort(cause);
            }
            drop((removed, run, candidate));
            return;
        }
        let removed = {
            let mut services = self.services.borrow_mut();
            let names: Vec<_> = services
                .iter()
                .filter(|(_, service)| {
                    service.owner == Some(journal.run)
                        && journal.services.contains(&service.ordinal)
                })
                .map(|(name, _)| name.clone())
                .collect();
            names
                .into_iter()
                .filter_map(|name| services.remove(&name))
                .collect::<Vec<_>>()
        };
        let run = self.facets.take_run(&journal.name, journal.run);
        // No scoped factory can have started while the enclosing declaration batch is held.
        // Extract before dropping jobs/captures so reentrant destructors never see a map borrow.
        let pending = std::mem::take(&mut *self.facets.pending.borrow_mut());
        let (withdrawn, retained): (Vec<_>, Vec<_>) = pending.into_iter().partition(|job| {
            journal
                .services
                .iter()
                .any(|id| id.0 == job.object.service_ordinal)
        });
        *self.facets.pending.borrow_mut() = retained;
        let startups: Vec<_> = {
            let mut startups = self.facets.startups.borrow_mut();
            journal
                .services
                .iter()
                .filter_map(|id| startups.remove(id))
                .collect()
        };
        let objects: Vec<_> = self
            .objects
            .borrow()
            .values()
            .filter(|object| {
                journal
                    .services
                    .iter()
                    .any(|id| id.0 == object.service_ordinal)
            })
            .cloned()
            .collect();
        // Withdraw storage authority before any cancellation waker can reenter and install
        // another service at the same address. New owners must never reuse this old ticket.
        let removed_objects = {
            let mut installed = self.objects.borrow_mut();
            objects
                .iter()
                .filter_map(|object| {
                    object.host_pin.set(false);
                    *object.error.borrow_mut() = Some(cause.clone());
                    object.phase.set(Phase::Disposed);
                    if installed
                        .get(&object.key)
                        .is_some_and(|current| Rc::ptr_eq(current, object))
                    {
                        installed.remove(&object.key)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
        };
        let factories: Vec<_> = objects
            .iter()
            .filter_map(|object| object.factory.borrow_mut().take())
            .collect();
        let activation_services = objects
            .iter()
            .filter_map(|object| object.activation_service.borrow_mut().take())
            .collect::<Vec<_>>();
        if let Some(run) = &run {
            run.abort(cause.clone());
        }
        for object in objects {
            object.cancellation.cancel(cause.clone());
            object.state.terminate(cause.clone());
            object.finish_ready(Err(cause.clone()));
            let waiters = std::mem::take(&mut *object.dispose_waiters.borrow_mut());
            for waiter in waiters {
                let _ = waiter.send(());
            }
        }
        for startup in startups {
            let _ = startup.settle(Err(cause.clone()));
        }
        drop((
            withdrawn,
            removed_objects,
            removed,
            run,
            factories,
            activation_services,
        ));
    }
    pub(super) fn start_host_service(
        self: &Rc<Self>,
        id: InstalledServiceId,
    ) -> Result<Option<Rc<HostActivation>>> {
        let service = self
            .services
            .borrow()
            .values()
            .find(|service| service.ordinal == id)
            .cloned()
            .ok_or_else(|| {
                RuntimeError::unavailable("unknown_type", "service disappeared during setup")
            })?;
        if service.options.lifetime != Lifetime::Host {
            return Ok(None);
        }
        let startup = self
            .facets
            .startups
            .borrow()
            .get(&id)
            .cloned()
            .expect("host service admission installs its startup ledger before acquisition");
        let reference = self.hold(
            service.contract,
            serde_json::json!({}),
            GetOptions::default(),
        )?;
        reference
            .claim
            .local_object()
            .expect("host singleton is local")
            .host_pin
            .set(true);
        reference.release();
        Ok(Some(startup))
    }
    pub(super) fn schedule_activation(
        self: &Rc<Self>,
        object: Rc<ObjectRecord>,
        previous: Option<Rc<ObjectRecord>>,
    ) {
        self.facets
            .pending
            .borrow_mut()
            .push(PendingActivation { object, previous });
        self.flush_declarations();
    }
    pub(super) fn flush_declarations(self: &Rc<Self>) {
        if std::thread::panicking() || self.facets.depth.get() != 0 || self.in_factory.get() != 0 {
            return;
        }
        let jobs = std::mem::take(&mut *self.facets.pending.borrow_mut());
        for job in jobs {
            if self.closing.get() {
                job.object.cancellation.cancel(RuntimeError::new(
                    ErrorCode::Gone,
                    "host_closed",
                    "Host closed",
                ));
            }
            let host = self.clone();
            self.driver.spawn(
                async move {
                    if let Some(previous) = job.previous {
                        previous.disposed().await;
                    }
                    host.activate(job.object).await;
                }
                .boxed_local(),
            );
        }
    }
    pub(super) fn ready_snapshot(self: &Rc<Self>) -> LocalBoxFuture<'static, Result<()>> {
        // Selection precedes a safe flush: an immediate-poll Driver may add later services.
        let selected = self.facets.startups.borrow().values().cloned().collect();
        self.flush_declarations();
        readiness(selected)
    }
    pub(super) fn report_startup(&self, startup: &HostActivation, result: Result<()>) {
        if let Some(error) = startup.settle(result) {
            self.errors.borrow_mut().push(error);
        }
    }
    pub(super) fn finalize_activation(self: &Rc<Self>, object: &Rc<ObjectRecord>) -> Result<()> {
        object.cancellation.check()?;
        if object.references.get() == 0 && !object.host_pin.get() {
            return Err(RuntimeError::aborted("abandoned", "activation abandoned"));
        }
        let mut state = object.state.prepare_activation()?;
        let declarations = object.registrations.borrow().clone();
        let mut tables = self.registries.prepare_activation(
            object,
            &declarations,
            object.current_epoch.get(),
        )?;
        state.preflight()?;
        self.registries.preflight(&tables)?;
        object.cancellation.check()?;
        let service = object
            .activation_service
            .borrow()
            .clone()
            .expect("activation service retained");
        let implementation = object
            .implementation
            .borrow()
            .clone()
            .expect("validated implementation");
        let installed = Rc::new(super::reload::InstalledVersion {
            epoch: self.epochs.exact(object.current_epoch.get())?,
            service,
            implementation,
            declarations: Rc::new(super::reload::DeclarationSet {
                dependencies: object.dependencies.borrow().clone(),
                resources: crate::resource::ResourceSet::new(
                    Rc::downgrade(self),
                    object.resources.borrow().clone(),
                    object.opened.borrow().clone(),
                ),
                state: object.state.declarations(),
                activities: object.activities.clone(),
                activity_runs: RefCell::new(None),
                registrations: declarations,
            }),
            adapters: RefCell::new(HashMap::new()),
        });
        let mut activities = object
            .activities
            .prepare(self, object, installed.clone(), None)?;
        activities.preflight()?;
        state.preflight()?;
        self.registries.preflight(&tables)?;
        let hold = self.publication_queue.hold();
        let (runs, activity_actions) = activities.adopt();
        *installed.declarations.activity_runs.borrow_mut() = Some(runs);
        object
            .installed
            .borrow_mut()
            .insert(object.current_epoch.get(), installed);
        let registry_events = self.registries.adopt(&mut tables);
        let state_events = state.adopt();
        object.phase.set(Phase::Live);
        self.publication_queue
            .append_registry_batch(registry_events);
        self.publication_queue.append_batch(state_events);
        state.finish();
        if let Some(startup) = &object.startup {
            self.report_startup(startup, Ok(()));
        }
        object.finish_ready(Ok(()));
        tables.finish();
        drop(hold);
        activity_actions.start();
        Ok(())
    }
    fn validate_service(&self, contract: &Contract, options: &ServeOptions) -> Result<()> {
        if let Some(placement) = contract.placement() {
            let allowed = match placement {
                tembra_contracts::Placement::Named { name } => name == self.name.as_str(),
                tembra_contracts::Placement::Family { prefix, .. } => self
                    .name
                    .strip_prefix(prefix)
                    .is_some_and(|suffix| !suffix.is_empty()),
            };
            if !allowed {
                return Err(RuntimeError::invalid(
                    "placement",
                    "contract is placed on another host",
                ));
            }
        }
        if options.lifetime == Lifetime::Host {
            if contract.kind() != ContractKind::Local {
                return Err(RuntimeError::invalid(
                    "lifetime",
                    "host lifetime requires a local contract",
                ));
            }
            contract
                .normalize_input(&serde_json::json!({}))
                .map_err(contract_error)?;
            if !options.linger.is_zero() {
                return Err(RuntimeError::invalid(
                    "lifetime",
                    "host objects cannot linger",
                ));
            }
        }
        check_binding(contract, contract, options)?;
        Ok(())
    }

    pub(super) fn insert_service(
        self: &Rc<Self>,
        contract: Rc<Contract>,
        factory: Factory,
        options: ServeOptions,
        owner: Option<FacetRunId>,
    ) -> Result<InstalledServiceId> {
        if self.closing.get() {
            return Err(RuntimeError::unavailable("closed", "host is closing"));
        }
        if self.services.borrow().contains_key(contract.name()) {
            return Err(RuntimeError::invalid("duplicate", "type is already served"));
        }
        self.validate_service(&contract, &options)?;
        let ordinal = InstalledServiceId(checked_next(&self.facets.next_service)?);
        if options.lifetime == Lifetime::Host {
            self.facets
                .startups
                .borrow_mut()
                .insert(ordinal, HostActivation::new());
        }
        self.services.borrow_mut().insert(
            contract.name().into(),
            Service {
                contract,
                factory,
                options,
                ordinal,
                owner,
            },
        );
        Ok(ordinal)
    }
}
pub(super) struct RetiredFacets {
    runs: Vec<Rc<InstalledRun>>,
    services: Vec<Service>,
}
impl RetiredFacets {
    pub(super) fn finish(self) {
        for run in &self.runs {
            run.abort(RuntimeError::aborted(
                "facet_replaced",
                "facet generation replaced or removed",
            ));
        }
        drop(self.services);
    }
}
pub(super) struct PreparedFacetBatch {
    pub(super) services: HashMap<String, Service>,
    pub(super) changed: Vec<String>,
    pub(super) removed: Vec<String>,
    runs: Vec<Rc<InstalledRun>>,
    pub(super) replaced_runs: Vec<String>,
    replaced_owners: Rc<HashSet<FacetRunId>>,
}
impl PreparedFacetBatch {
    pub(super) fn abort(&self, cause: RuntimeError) {
        for run in &self.runs {
            run.abort(cause.clone());
        }
    }
}
impl HostInner {
    pub(super) fn prepare_reload_facets(
        self: &Rc<Self>,
        definitions: Vec<ExecutableFacet>,
        removed: &[String],
    ) -> Result<PreparedFacetBatch> {
        self.epochs.check()?;
        if self.closing.get() {
            return Err(RuntimeError::unavailable("closed", "Host is closing"));
        }
        let mut names = HashSet::new();
        for name in definitions
            .iter()
            .map(ExecutableFacet::name)
            .chain(removed.iter().map(String::as_str))
        {
            if !names.insert(name.to_owned()) {
                return Err(RuntimeError::invalid(
                    "duplicate_facet",
                    "reload facet occurs twice",
                ));
            }
            if self
                .facets
                .runs
                .borrow()
                .get(name)
                .is_some_and(|run| run.definition.kind() == crate::FacetKind::Native)
            {
                return Err(RuntimeError::invalid(
                    "native_reload",
                    "native facet cannot be replaced or removed",
                ));
            }
        }
        if definitions
            .iter()
            .any(|definition| definition.kind() != crate::FacetKind::Foreign)
        {
            return Err(RuntimeError::invalid(
                "native_reload",
                "reload accepts foreign definitions only",
            ));
        }
        let candidate = Rc::new(RefCell::new(HashMap::new()));
        let replaced_owners = Rc::new(
            self.facets
                .runs
                .borrow()
                .iter()
                .filter(|(name, _)| names.contains(*name))
                .map(|(_, run)| run.id)
                .collect(),
        );
        let mut prepared = PreparedFacetBatch {
            services: HashMap::new(),
            changed: definitions
                .iter()
                .map(|definition| definition.name().into())
                .collect(),
            removed: removed.to_vec(),
            runs: Vec::new(),
            replaced_runs: names.into_iter().collect(),
            replaced_owners,
        };
        for definition in definitions {
            let id = FacetRunId(checked_next(&self.facets.next_run)?);
            let run = InstalledRun::prepare(id, definition);
            let journal = Rc::new(RefCell::new(SetupJournal {
                host: Rc::downgrade(self),
                run: id,
                name: run.definition.name().into(),
                services: Vec::new(),
                active: true,
                candidate: Some(candidate.clone()),
                candidate_run: Some(run.clone()),
                candidate_replaced: Some(prepared.replaced_owners.clone()),
            }));
            let scope = FacetScope::new(self, FacetSignal::new(run.cancellation.clone()));
            let setup = {
                let (_pass, weak) = SetupPass::enter(self);
                run.definition
                    .setup(&scope, &mut FacetDeclaration::new(self, &journal, weak))
            };
            let mut journal = Rc::try_unwrap(journal)
                .ok()
                .expect("weak setup ownership")
                .into_inner();
            if let Err(cause) = setup {
                self.withdraw_setup(&mut journal, cause.clone());
                prepared.abort(cause.clone());
                return Err(cause);
            }
            journal.active = false;
            prepared.runs.push(run);
        }
        prepared.services = std::mem::take(&mut *candidate.borrow_mut());
        Ok(prepared)
    }
    pub(super) fn reload_removed_services(&self, batch: &PreparedFacetBatch) -> Vec<String> {
        let ids = self
            .facets
            .runs
            .borrow()
            .iter()
            .filter(|(name, _)| batch.replaced_runs.contains(name))
            .map(|(_, run)| run.id)
            .collect::<Vec<_>>();
        self.services
            .borrow()
            .iter()
            .filter(|(name, service)| {
                service.owner.is_some_and(|owner| ids.contains(&owner))
                    && !batch.services.contains_key(*name)
            })
            .map(|(name, _)| name.clone())
            .collect()
    }

    pub(super) fn adopt_reload_facets(&self, batch: &mut PreparedFacetBatch) -> RetiredFacets {
        let old_ids = self
            .facets
            .runs
            .borrow()
            .iter()
            .filter(|(name, _)| batch.replaced_runs.contains(name))
            .map(|(_, run)| run.id)
            .collect::<Vec<_>>();
        let retired_services = {
            let mut services = self.services.borrow_mut();
            let names = services
                .iter()
                .filter(|(_, service)| service.owner.is_some_and(|owner| old_ids.contains(&owner)))
                .map(|(name, _)| name.clone())
                .collect::<Vec<_>>();
            let retired = names
                .into_iter()
                .filter_map(|name| services.remove(&name))
                .collect::<Vec<_>>();
            services.extend(batch.services.drain());
            retired
        };
        // Caller retains returned runs and service captures through the callback-free adoption.
        let retired = {
            let mut runs = self.facets.runs.borrow_mut();
            let retired = batch
                .replaced_runs
                .iter()
                .filter_map(|name| runs.remove(name))
                .collect::<Vec<_>>();
            for run in &batch.runs {
                runs.insert(run.definition.name().into(), run.clone());
            }
            retired
        };
        RetiredFacets {
            runs: retired,
            services: retired_services,
        }
    }
}

fn checked_next(counter: &Cell<u64>) -> Result<u64> {
    let next = counter
        .get()
        .checked_add(1)
        .filter(|value| *value <= 9_007_199_254_740_991)
        .ok_or_else(|| {
            RuntimeError::unavailable("declaration_id", "declaration identity space exhausted")
        })?;
    counter.set(next);
    Ok(next)
}

impl HostInner {
    pub(super) fn check_reload_service_ownership(&self, batch: &PreparedFacetBatch) -> Result<()> {
        let services = self.services.borrow();
        if batch.services.keys().any(|name| {
            services.get(name).is_some_and(|service| {
                !service
                    .owner
                    .is_some_and(|owner| batch.replaced_owners.contains(&owner))
            })
        }) {
            return Err(RuntimeError::invalid(
                "duplicate",
                "candidate type acquired an unreplaced service owner",
            ));
        }
        Ok(())
    }
    pub(super) fn check_reload_services(&self, batch: &PreparedFacetBatch) -> Result<()> {
        self.check_reload_service_ownership(batch)?;
        let current = self.services.borrow().clone();
        let mut prospective = current.clone();
        for name in self.reload_removed_services(batch) {
            prospective.remove(&name);
        }
        prospective.extend(batch.services.clone());
        for (name, next) in &batch.services {
            let Some(previous) = current.get(name) else {
                continue;
            };
            if next.contract.kind() != previous.contract.kind()
                || next.contract.placement() != previous.contract.placement()
            {
                return Err(RuntimeError::new(
                    ErrorCode::ContractMismatch,
                    "placement",
                    "reload cannot change contract kind or placement",
                ));
            }
            tembra_contracts::identity_compatible(previous.contract.input(), next.contract.input())
                .map_err(|error| {
                    RuntimeError::new(ErrorCode::ContractMismatch, error.reason, error.message)
                })?;
            if next.contract.version() == previous.contract.version() {
                check_binding(&next.contract, &previous.contract, &next.options)?;
            }
        }
        let mut uses = self
            .facets
            .runs
            .borrow()
            .iter()
            .filter(|(name, _)| !batch.replaced_runs.contains(name))
            .flat_map(|(_, run)| run.definition.uses().to_vec())
            .collect::<Vec<_>>();
        uses.extend(
            batch
                .runs
                .iter()
                .flat_map(|run| run.definition.uses().to_vec()),
        );
        for token in uses {
            if batch.services.contains_key(token.name())
                && let Some(service) = prospective.get(token.name())
            {
                check_binding(&service.contract, &token, &service.options)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod run_ownership_tests {
    use super::*;

    #[test]
    fn actual_installed_run_survives_prepare_abort_and_native_alias_until_commit() {
        let driver = crate::ManualDriver::new();
        let host = Host::new(
            crate::HostOptions::new("runs", serde_json::json!(null)),
            driver.clone(),
        );
        let definition = NativeFacet::new("run", vec![], |_, _| Ok(())).unwrap();
        driver
            .run_until(
                host.install(definition.clone(), LoadOptions::default())
                    .unwrap(),
            )
            .unwrap();
        let installed = host.inner.facets.runs.borrow()["run"].clone();
        let candidate = InstalledRun::prepare(FacetRunId(2), definition.clone());
        assert!(!installed.cancellation.is_cancelled());
        let cause = RuntimeError::aborted("candidate_refused", "candidate refused");
        candidate.abort(cause.clone());
        assert_eq!(driver.run_until(candidate.cancellation.cancelled()), cause);
        assert!(!installed.cancellation.is_cancelled());
        // Re-associating the exact same native owner never replaces or cancels it.
        host.inner.facets.commit_run(installed.clone());
        assert!(Rc::ptr_eq(
            &host.inner.facets.runs.borrow()["run"],
            &installed
        ));
        assert!(!installed.cancellation.is_cancelled());
        let replacement = InstalledRun::prepare(FacetRunId(3), definition);
        host.inner.facets.commit_run(replacement.clone());
        assert!(installed.cancellation.is_cancelled());
        assert!(!replacement.cancellation.is_cancelled());
        assert!(host.inner.facets.take_run("run", installed.id).is_none());
        let removed = host.inner.facets.take_run("run", replacement.id).unwrap();
        removed.abort(RuntimeError::new(
            ErrorCode::Gone,
            "facet_removed",
            "facet removed",
        ));
        assert!(replacement.cancellation.is_cancelled());
        driver.run_until(host.close()).unwrap();
    }
}
