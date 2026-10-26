//! Update ownership indexes with the records they protect.

use super::*;

impl Inner {
    /// Resolve the facet owner responsible for an owned task.
    pub fn task_facet(&self, owner: Owner) -> Option<Id> {
        match owner {
            Owner::Facet(id) => Some(id),
            Owner::Object(id) => self
                .objects
                .get(&id)
                .map(|object| self.facets[&object.facet].owner),
            Owner::Host => None,
        }
    }

    /// Remove one program from its indexed deadline bucket.
    pub fn remove_deadline(&mut self, deadline: Option<u64>, id: Id) {
        if let Some(deadline) = deadline
            && let Some(ids) = self.program_deadlines.get_mut(&deadline)
        {
            ids.remove(&id);
            if ids.is_empty() {
                self.program_deadlines.remove(&deadline);
            }
        }
    }

    /// Remove program leases, waiters and deadline memberships together.
    pub fn remove_program(&mut self, id: Id) {
        if let Some(program) = self.programs.remove(&id) {
            for pair in &program.replacements {
                for scope in [pair.previous, pair.next].into_iter().flatten() {
                    self.scope_programs.remove(&scope, id);
                }
            }
            if let Some(scope) = program.activation_scope {
                self.scope_activations.remove(&scope);
                self.wake(WaitKey::Scope(scope));
            }
            for key in program.waiting {
                self.waiters.remove(&key, id);
            }
            self.remove_deadline(program.deadline, id);
            self.wake(WaitKey::Program(id));
        }
        self.ready_programs.remove(&id);
    }

    /// Queue only programs indexed under the changed key.
    pub fn wake(&mut self, key: WaitKey) {
        self.ready_programs.extend(self.waiters.take(&key));
    }

    /// Wake owner cleanup when a physical user settles.
    pub fn settled_owner(&mut self, owner: Owner) {
        self.wake(WaitKey::Owner(owner));
        if owner != Owner::Host {
            self.wake(WaitKey::Owner(Owner::Host));
        }
    }

    /// Reserve an identity from this host's shared allocator.
    pub fn id(&mut self) -> Result<Id> {
        reserve(&mut self.next)
    }

    /// Read the published facet-name index.
    pub fn current(&self) -> &BTreeMap<String, Id> {
        &self.versions[&self.version]
    }

    /// Reject new admissions once host shutdown starts.
    pub fn open(&self) -> Result<()> {
        if self.closing {
            Err(Error::new(Code::Closed, Reason::Host, "host is closing"))
        } else {
            Ok(())
        }
    }

    /// Require an active root that has not been cancelled.
    pub fn check_root(&self, id: Id) -> Result<&Root> {
        let root = self
            .roots
            .get(&id)
            .ok_or_else(|| Error::new(Code::Expired, Reason::Root, "scope has ended"))?;
        if !root.active {
            return Err(Error::new(Code::Expired, Reason::Root, "scope has ended"));
        }
        if root.cancelled {
            return Err(Error::new(
                Code::Cancelled,
                Reason::Root,
                "scope is cancelled",
            ));
        }
        Ok(root)
    }

    /// Pin a known implementation version for one operation scope.
    pub(super) fn root(
        &mut self,
        version: Version,
        principal: String,
        cleanup: bool,
    ) -> Result<Id> {
        if !self.versions.contains_key(&version) {
            return Err(conflict(Reason::Superseded));
        }
        let id = self.id()?;
        *self.version_users.entry(version).or_default() += 1;
        self.roots.insert(
            id,
            Root {
                version,
                principal,
                active: true,
                cancelled: false,
                calls: 0,
                contexts: BTreeSet::new(),
                touched: BTreeSet::new(),
                facets: BTreeSet::new(),
                cleanup,
            },
        );
        Ok(id)
    }

    /// Resolve a service directly in the selected version index.
    pub(super) fn find_service(&self, name: &str, version: Version) -> Result<(Id, &Service)> {
        let services = self
            .services
            .get(&version)
            .ok_or_else(|| conflict(Reason::Superseded))?;
        services
            .get(name)
            .map(|(facet, service)| (*facet, service))
            .ok_or_else(|| Error::new(Code::Unavailable, Reason::UnknownService, name))
    }

    /// Reject calls below the facet's version or admission gate.
    pub(super) fn check_facet(&self, facet: Id, root: Id, version: Version) -> Result<Id> {
        let owner = self
            .facets
            .get(&facet)
            .ok_or_else(|| gone(Reason::Facet))?
            .owner;
        if self
            .facet_min
            .get(&owner)
            .is_some_and(|minimum| version < *minimum)
            || self
                .pending
                .as_ref()
                .and_then(|pending| pending.gated_facets.get(&owner))
                .is_some_and(|allowed| !allowed.contains(&root))
        {
            return Err(conflict(Reason::UpgradeConflict));
        }
        Ok(owner)
    }

    /// Check indexed root and task ownership for a facet.
    pub(super) fn facet_busy(&self, owner: Id) -> bool {
        self.facet_roots.contains(&owner) || self.facet_tasks.contains(&owner)
    }

    /// Check whether an object still has roots or tasks.
    pub(super) fn gate_object_busy(&self, object: Id) -> bool {
        self.object_roots.contains(&object) || self.objects[&object].tasks != 0
    }

    /// Require a live host, facet or object declaration owner.
    pub fn owner(&self, owner: Owner) -> Result<()> {
        match owner {
            Owner::Host => {
                if !self.closed {
                    Ok(())
                } else {
                    Err(gone(Reason::Host))
                }
            }
            Owner::Facet(id) => {
                if self.current_owners.contains(&id)
                    || self
                        .pending
                        .as_ref()
                        .is_some_and(|pending| pending.owners.contains(&id))
                {
                    Ok(())
                } else {
                    Err(gone(Reason::Facet))
                }
            }
            Owner::Object(id) => {
                if self
                    .objects
                    .get(&id)
                    .is_some_and(|o| !o.disposing && o.phase != Status::Gone)
                {
                    Ok(())
                } else {
                    Err(gone(Reason::Object))
                }
            }
        }
    }

    /// Check only indexed users within the relevant version range.
    pub(super) fn owner_busy(&self, owner: Owner, from: Version, before: Option<Version>) -> bool {
        let retained = |version| version >= from && before.is_none_or(|before| version < before);
        self.owner_calls
            .members(&owner)
            .any(|id| retained(self.calls[&id].code_version))
            || self
                .owner_tasks
                .members(&owner)
                .any(|id| retained(self.tasks[&id].version))
    }

    /// Schedule retirement only after all ownership and linger conditions pass.
    pub(crate) fn maybe_dispose(&mut self, id: Id) {
        if self.closing
            && (self.scope_dependencies.contains(&id) || self.object_dependents.contains(&id))
        {
            return;
        }
        let Some(object) = self.objects.get_mut(&id) else {
            return;
        };
        if object.disposing
            || (!self.closing && object.phase == Status::Loading)
            || object.preparing
            || object.pinned
            || object.calls != 0
            || (object.references != 0 && object.phase != Status::Gone && !self.closing)
        {
            return;
        }
        if !self.closing && object.phase == Status::Live && object.linger_ms != 0 {
            let since = *object.idle_at.get_or_insert(self.now);
            let deadline = since.saturating_add(u64::from(object.linger_ms));
            if self.now < deadline {
                self.idle.entry(deadline).or_default().insert(id);
                return;
            }
        }
        // Cancellation starts retirement; reacquisition must wait for physical task settlement.
        object.phase = Status::Gone;
        for task in self.owner_tasks.members(&Owner::Object(id)) {
            let record = self.tasks.get_mut(&task).expect("indexed task");
            if !record.cancelled {
                record.cancelled = true;
                self.actions.push_back(Action::CancelTask(task));
            }
        }
        if object.tasks != 0 {
            return;
        }
        object.disposing = true;
        self.wake(WaitKey::Object(id));
        self.actions.push_back(Action::DisposeObject(id));
    }

    /// Release one version pin and collect unused metadata.
    pub fn release_version(&mut self, version: Version) {
        let count = self
            .version_users
            .get_mut(&version)
            .expect("retained version");
        *count -= 1;
        if *count == 0 {
            self.version_users.remove(&version);
        }
        self.collect_version(version);
    }

    /// Remove root memberships and release its retained version.
    pub fn remove_root(&mut self, id: Id) -> Vec<crate::OpaqueContextValue> {
        let mut leases = Vec::new();
        if let Some(root) = self.roots.remove(&id) {
            for frame in root.contexts {
                if let Some(frame) = self.context_frames.remove(&frame)
                    && let crate::ContextValue::Opaque(lease) = frame.value
                {
                    leases.push(lease);
                }
            }
            for object in root.touched {
                self.object_roots.remove(&object, id);
                if self
                    .pending
                    .as_ref()
                    .is_some_and(|pending| pending.gated.contains(&object))
                {
                    self.wake(WaitKey::Owner(Owner::Object(object)));
                }
            }
            for facet in root.facets {
                self.facet_roots.remove(&facet, id);
                if self
                    .pending
                    .as_ref()
                    .is_some_and(|pending| pending.gated_facets.contains_key(&facet))
                {
                    self.wake(WaitKey::Owner(Owner::Facet(facet)));
                }
            }
            self.release_version(root.version);
        }
        leases
    }

    /// Retire facet callbacks after all version and object claims end.
    pub fn collect_facet(&mut self, id: Id) {
        if !self.facet_versions.contains(&id)
            && !self.facet_objects.contains(&id)
            && self.facets.remove(&id).is_some()
        {
            self.actions.push_back(Action::RetireFacet(id));
            #[cfg(feature = "hooks")]
            self.contributions.retain(|c| c.facet != id);
        }
    }

    /// Release an unused version and its cached hook plans.
    pub fn collect_version(&mut self, version: Version) {
        if version == self.version || self.version_users.contains_key(&version) {
            return;
        }
        if let Some(facets) = self.versions.remove(&version) {
            self.services.remove(&version);
            for id in facets.into_values() {
                self.facet_versions.remove(&id, version.into());
                self.collect_facet(id);
            }
            #[cfg(feature = "hooks")]
            {
                let keys: Vec<_> = self
                    .plans
                    .range((version, String::new(), String::new())..)
                    .take_while(|((v, _, _), _)| *v == version)
                    .map(|(key, _)| key.clone())
                    .collect();
                for key in keys {
                    self.plans.remove(&key);
                }
            }
        }
    }
}
