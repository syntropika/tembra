use super::*;

impl Host {
    /// Reserves one opaque adapter value, callback or source identity in the host's namespace.
    pub fn allocate_handle(&self) -> Result<Id> {
        self.inner.borrow_mut().id()
    }
    pub fn new() -> Self {
        Self {
            inner: Rc::new(RefCell::new(Inner {
                next: 1,
                version: 1,
                closing: false,
                closed: false,
                facets: BTreeMap::new(),
                versions: BTreeMap::from([(1, BTreeMap::new())]),
                services: BTreeMap::from([(1, BTreeMap::new())]),
                objects: BTreeMap::new(),
                addresses: BTreeMap::new(),
                references: BTreeMap::new(),
                object_references: Index::default(),
                roots: BTreeMap::new(),
                calls: BTreeMap::new(),
                resources: BTreeMap::new(),
                tasks: BTreeMap::new(),
                actions: VecDeque::new(),
                pending: None,
                now: 0,
                idle: BTreeMap::new(),
                facet_min: BTreeMap::new(),
                scopes: BTreeMap::new(),
                programs: BTreeMap::new(),
                ready_programs: BTreeSet::new(),
                program_deadlines: BTreeMap::new(),
                waiters: Index::default(),
                owner_scopes: Index::default(),
                scope_versions: VersionCounts::default(),
                scope_code_versions: BTreeMap::new(),
                scope_activations: BTreeMap::new(),
                non_host_scopes: 0,
                resource_versions: VersionCounts::default(),
                current_owners: BTreeSet::new(),
                facet_scopes: Index::default(),
                scope_children: Index::default(),
                resource_scopes: Index::default(),
                resource_claims: Index::default(),
                owner_resources: Index::default(),
                resource_names: Index::default(),
                dependents: Index::default(),
                owner_tasks: Index::default(),
                owner_calls: Index::default(),
                facet_objects: Index::default(),
                facet_versions: Index::default(),
                version_users: BTreeMap::new(),
                object_roots: Index::default(),
                facet_roots: Index::default(),
                facet_tasks: Index::default(),
                object_dependents: Index::default(),
                scope_dependencies: Index::default(),
                reference_scopes: Index::default(),
                #[cfg(test)]
                program_visits: 0,
                #[cfg(feature = "hooks")]
                contributions: Vec::new(),
                #[cfg(feature = "hooks")]
                plans: BTreeMap::new(),
                #[cfg(feature = "subscriptions")]
                subscriptions: BTreeMap::new(),
                #[cfg(feature = "subscriptions")]
                scope_listeners: Index::default(),
                #[cfg(feature = "subscriptions")]
                owner_listeners: Index::default(),
                #[cfg(feature = "subscriptions")]
                ready_deliveries: BTreeSet::new(),
                #[cfg(all(test, feature = "subscriptions"))]
                delivery_visits: 0,
            })),
        }
    }

    /// Completes adapter cleanup. In-flight calls/tasks and owned resources must already settle.
    pub fn disposed(&self, object: Id) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        let record = inner.objects.get(&object).ok_or_else(|| gone("object"))?;
        if record.calls != 0
            || record.tasks != 0
            || inner.owner_resources.contains(&Owner::Object(object))
        {
            return Err(conflict("cleanup_busy"));
        }
        if !record.disposing {
            return Err(conflict("object_state"));
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
        inner.facet_objects.remove(&record.facet, object);
        inner.collect_facet(record.facet);
        for id in targets {
            inner.maybe_dispose(id);
        }
        Ok(())
    }

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
            return Err(Error::invalid("clock", "clock must be monotonic"));
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

    pub fn close(&self) {
        let mut inner = self.inner.borrow_mut();
        if inner.closing {
            return;
        }
        inner.closing = true;
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
            return Err(conflict("cleanup_busy"));
        }
        inner.closed = true;
        inner.references.clear();
        inner.object_references = Index::default();
        inner.scopes.clear();
        inner.programs.clear();
        inner.roots.clear();
        inner.version_users.clear();
        inner.object_roots = Index::default();
        inner.facet_roots = Index::default();
        inner.facet_versions = Index::default();
        inner.facets.clear();
        inner.facet_min.clear();
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

    pub fn drain(&self) -> Vec<Action> {
        self.pump_programs();
        #[cfg(feature = "subscriptions")]
        self.pump_deliveries();
        self.inner.borrow_mut().actions.drain(..).collect()
    }

    pub fn diagnostics(&self) -> Diagnostics {
        let inner = self.inner.borrow();
        Diagnostics {
            objects: inner.objects.len(),
            references: inner.references.values().filter(|r| !r.released).count(),
            roots: inner.roots.len(),
            calls: inner.calls.len(),
            resources: inner.resources.len(),
            tasks: inner.tasks.len(),
            facets: inner.current().len(),
            closing: inner.closing,
        }
    }
}
