use super::*;
use crate::Reason;

impl Host {
    /// Reserves one opaque adapter value, callback or source identity in the host's namespace.
    ///
    /// # Errors
    /// After [`MAX_ID`] is issued, returns `exhausted/identities` without advancing the counter.
    pub fn allocate_handle(&self) -> Result<Id> {
        self.inner.borrow_mut().id()
    }

    /// Create an empty kernel with no platform dependencies.
    pub fn new() -> Self {
        Self {
            inner: Rc::new(RefCell::new(Inner {
                next: 1,
                version: 1,
                versions: BTreeMap::from([(1, BTreeMap::new())]),
                services: BTreeMap::from([(1, BTreeMap::new())]),
                ..Inner::default()
            })),
        }
    }

    /// Completes adapter cleanup. In-flight calls/tasks and owned resources must already settle.
    pub fn disposed(&self, object: Id) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        let record = inner
            .objects
            .get(&object)
            .ok_or_else(|| gone(Reason::Object))?;
        if record.calls != 0
            || record.tasks != 0
            || inner.owner_resources.contains(&Owner::Object(object))
        {
            return Err(conflict(Reason::CleanupBusy));
        }
        if !record.disposing {
            return Err(conflict(Reason::ObjectState));
        }
        let record = inner.objects.remove(&object).expect("checked object");
        inner.addresses.remove(&record.address);
        let mut targets = BTreeSet::new();
        for reference in &record.dependencies {
            if let Some(target) = inner.references.get(reference).map(|claim| claim.object) {
                inner.object_dependents.remove(&target, *reference);
            }
            if let Some(claim) = inner.references.get_mut(reference)
                && !claim.released
            {
                claim.released = true;
                let target = claim.object;
                targets.insert(target);
                if let Some(target_record) = inner.objects.get_mut(&target) {
                    target_record.references -= 1;
                }
            }
        }
        if let Some(state) = record.state {
            inner.actions.push_back(Action::DropValue(state.value));
        }
        #[cfg(feature = "subscriptions")]
        {
            let listeners = inner
                .owner_listeners
                .members(&Owner::Object(object))
                .collect::<Vec<_>>();
            for listener in listeners {
                inner.remove_subscription(listener);
            }
        }
        inner.orb_objects.remove(&record.orb, object);
        inner.collect_orb(record.orb);
        for id in targets {
            inner.maybe_dispose(id);
        }
        Ok(())
    }

    /// Read the earliest indexed object retirement deadline.
    pub fn next_idle(&self) -> Option<u64> {
        let inner = self.inner.borrow();
        inner
            .idle
            .first_key_value()
            .map(|(deadline, _)| deadline.saturating_sub(inner.now))
    }

    /// Advances adapter-provided monotonic time and admits expired idle disposal.
    pub fn tick(&self, now: u64) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        if now < inner.now {
            return Err(Error::invalid(Reason::Clock, "clock must be monotonic"));
        }
        inner.now = now;
        while inner
            .program_deadlines
            .first_key_value()
            .is_some_and(|(deadline, _)| *deadline <= now)
        {
            let (_, programs) = inner.program_deadlines.pop_first().expect("due deadline");
            inner.ready_programs.extend(programs);
        }
        let mut ids = Vec::new();
        while inner
            .idle
            .first_key_value()
            .is_some_and(|(deadline, _)| *deadline <= now)
        {
            ids.extend(inner.idle.pop_first().expect("checked deadline").1);
        }
        for id in ids {
            inner.maybe_dispose(id);
        }
        Ok(())
    }

    /// Stops admissions and cancels pending reloads. Their scopes remain reserved until physical
    /// effects settle; cleanup then runs in dependency order. Call `finish_close` after draining.
    pub fn close(&self) {
        let mut inner = self.inner.borrow_mut();
        if inner.closing {
            return;
        }
        inner.closing = true;
        let programs = inner
            .programs
            .iter_mut()
            .filter_map(|(id, program)| program.close_reload().then_some(*id))
            .collect::<Vec<_>>();
        inner.ready_programs.extend(programs);
        let unowned = inner
            .pending
            .as_ref()
            .filter(|pending| {
                !inner
                    .programs
                    .values()
                    .any(|program| program.ticket == Some(pending.reload.id))
            })
            .map(|pending| pending.reload.id);
        drop(inner);
        if let Some(ticket) = unowned {
            self.abort(ticket)
                .expect("pending reload without a program");
        }
        let mut inner = self.inner.borrow_mut();
        inner.idle.clear();
        let roots: Vec<_> = inner.roots.keys().copied().collect();
        for root in roots {
            let record = inner.roots.get_mut(&root).expect("root exists");
            record.cancelled = true;
            inner.actions.push_back(Action::CancelRoot(root));
        }
        let tasks: Vec<_> = inner.tasks.keys().copied().collect();
        for task in tasks {
            inner.tasks.get_mut(&task).expect("task exists").cancelled = true;
            inner.actions.push_back(Action::CancelTask(task));
        }
        // Stop admissions first. The shared program drains scopes in dependency order.
        let objects: Vec<_> = inner.objects.keys().copied().collect();
        for object in objects {
            if inner.objects[&object].phase == Status::Loading {
                let scopes: Vec<_> = inner.owner_scopes.members(&Owner::Object(object)).collect();
                inner.begin_cleanup(&scopes);
            }
            let record = inner.objects.get_mut(&object).expect("object exists");
            record.pinned = false;
            inner.maybe_dispose(object);
        }
        let scopes = inner
            .scopes
            .values()
            .filter(|scope| !matches!(scope.owner, Owner::Object(_)) && !scope.closing)
            .map(|scope| scope.id)
            .collect::<Vec<_>>();
        drop(inner);
        if !scopes.is_empty() {
            match self
                .cleanup_scopes(scopes, None)
                .and_then(|program| self.run_program(program))
            {
                Ok(()) => {}
                Err(error) => self
                    .inner
                    .borrow_mut()
                    .actions
                    .push_back(Action::ProgramComplete {
                        program: 0,
                        error: Some(error),
                    }),
            }
        }
    }

    /// Release kernel storage only after physical ownership settles.
    pub fn finish_close(&self) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        if !inner.closing
            || !inner.objects.is_empty()
            || !inner.calls.is_empty()
            || !inner.tasks.is_empty()
            || !inner.resources.is_empty()
            || !inner.scopes.is_empty()
            || !inner.programs.is_empty()
        {
            return Err(conflict(Reason::CleanupBusy));
        }
        inner.closed = true;
        inner.references.clear();
        inner.object_references = Index::default();
        inner.scopes.clear();
        inner.programs.clear();
        inner.roots.clear();
        inner.version_users.clear();
        inner.object_roots = Index::default();
        inner.orb_roots = Index::default();
        inner.orb_versions = Index::default();
        inner.orbs.clear();
        inner.orb_min.clear();
        inner.current_owners.clear();
        inner.idle.clear();
        #[cfg(feature = "hooks")]
        {
            inner.contributions.clear();
            inner.plans.clear();
        }
        #[cfg(feature = "subscriptions")]
        {
            inner.subscriptions.clear();
            inner.scope_listeners = Index::default();
            inner.owner_listeners = Index::default();
            inner.ready_deliveries.clear();
        }
        let version = inner.version;
        inner.versions = BTreeMap::from([(version, BTreeMap::new())]);
        inner.services = BTreeMap::from([(version, BTreeMap::new())]);
        Ok(())
    }

    /// Advance ready work and return pending adapter actions.
    pub fn drain(&self) -> Vec<Action> {
        self.pump_programs();
        #[cfg(feature = "subscriptions")]
        self.pump_deliveries();
        self.inner.borrow_mut().actions.drain(..).collect()
    }

    /// Count retained kernel records without activating services.
    pub fn diagnostics(&self) -> Diagnostics {
        let inner = self.inner.borrow();
        Diagnostics {
            objects: inner.objects.len(),
            references: inner.references.values().filter(|r| !r.released).count(),
            roots: inner.roots.len(),
            calls: inner.calls.len(),
            resources: inner.resources.len(),
            tasks: inner.tasks.len(),
            orbs: inner.current().len(),
            closing: inner.closing,
        }
    }
}
