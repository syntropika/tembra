use super::*;
use crate::Reason;

impl Host {
    /// Prepares an isolated replacement. No installed declaration or object state changes.
    ///
    /// # Errors
    /// Refuses concurrent preparation, unavailable requirements, ambiguous services, unsupported
    /// features, exhausted identities, and contracts incompatible with surviving references.
    pub fn prepare(&self, declarations: Vec<Facet>, remove: Vec<String>) -> Result<Reload> {
        let mut inner = self.inner.borrow_mut();
        inner.open()?;
        if inner.pending.is_some() {
            return Err(conflict(Reason::ReloadRunning));
        }
        if inner.versions.len() >= 32 {
            return Err(conflict(Reason::RetainedVersions));
        }
        let version = inner.version.checked_add(1).ok_or_else(exhausted)?;
        let mut current = inner.current().clone();
        let mut names = BTreeSet::new();
        for name in &remove {
            if !names.insert(name.clone()) {
                return Err(Error::invalid(Reason::Duplicate, name));
            }
            current.remove(name);
        }
        let mut facets = Vec::new();
        let mut next = inner.next;
        for declaration in declarations {
            validate_facet(&declaration)?;
            if !names.insert(declaration.name.clone()) {
                return Err(Error::invalid(Reason::Duplicate, &declaration.name));
            }
            let id = reserve(&mut next)?;
            let owner = inner
                .current()
                .get(&declaration.name)
                .map(|id| inner.facets[id].owner)
                .unwrap_or(id);
            current.insert(declaration.name.clone(), id);
            facets.push(InstalledFacet {
                id,
                owner,
                code_version: version,
                declaration,
            });
        }
        let lookup = |id: &Id| {
            facets
                .iter()
                .find(|f| f.id == *id)
                .or_else(|| inner.facets.get(id))
        };
        let mut services = BTreeMap::new();
        for id in current.values() {
            let facet = lookup(id).expect("prepared facet exists");
            for service in &facet.declaration.services {
                if services.insert(service.name.clone(), facet).is_some() {
                    return Err(Error::invalid(Reason::DuplicateService, &service.name));
                }
            }
        }
        for id in current.values() {
            let facet = lookup(id).expect("prepared facet exists");
            for requirement in &facet.declaration.requires {
                let provider = current
                    .get(&requirement.facet)
                    .and_then(lookup)
                    .ok_or_else(|| {
                        Error::new(Code::Unavailable, Reason::Dependency, &requirement.facet)
                    })?;
                if provider.declaration.api_version != requirement.api_version {
                    return Err(conflict(Reason::ApiVersion));
                }
                for (name, methods) in &requirement.methods {
                    let service = provider
                        .declaration
                        .services
                        .iter()
                        .find(|s| &s.name == name)
                        .ok_or_else(|| Error::invalid(Reason::DependencyService, name))?;
                    if !methods.is_subset(&service.methods) {
                        return Err(conflict(Reason::MissingMethod));
                    }
                }
                if !requirement.hooks.is_subset(&provider.declaration.hooks) {
                    return Err(conflict(Reason::MissingHook));
                }
            }
        }
        let objects: Vec<Id> = names
            .iter()
            .filter_map(|name| inner.current().get(name))
            .flat_map(|facet| inner.facet_objects.members(facet))
            .filter(|id| !inner.objects[id].disposing && inner.objects[id].phase != Status::Gone)
            .collect();
        for reference in objects
            .iter()
            .flat_map(|id| inner.object_references.members(id))
            .map(|id| &inner.references[&id])
            .filter(|r| !r.released)
        {
            let Some(object) = inner.objects.get(&reference.object) else {
                continue;
            };
            if object.disposing {
                continue;
            }
            let provider = services
                .get(&object.address.service)
                .ok_or_else(|| conflict(Reason::LiveConsumer))?;
            if reference.expected_api != 0
                && reference.expected_api != provider.declaration.api_version
            {
                return Err(conflict(Reason::ApiVersion));
            }
            let service = provider
                .declaration
                .services
                .iter()
                .find(|s| s.name == object.address.service)
                .expect("service exists");
            if !reference.methods.is_subset(&service.methods) {
                return Err(conflict(Reason::MissingMethod));
            }
        }
        if objects
            .iter()
            .any(|id| inner.objects[id].phase == Status::Loading)
        {
            return Err(conflict(Reason::ActivationBusy));
        }
        let services = services
            .into_iter()
            .map(|(name, facet)| {
                let service = facet
                    .declaration
                    .services
                    .iter()
                    .find(|service| service.name == name)
                    .expect("declared service")
                    .clone();
                (name, (facet.id, service, facet.declaration.api_version))
            })
            .collect();
        let ticket = reserve(&mut next)?;
        let reload = Reload {
            id: ticket,
            code_version: version,
            facets,
            objects,
        };
        inner.next = next;
        for object in &reload.objects {
            inner
                .objects
                .get_mut(object)
                .expect("checked object")
                .preparing = true;
        }
        inner.pending = Some(Pending {
            reload: reload.clone(),
            objects: reload.objects.iter().copied().collect(),
            current,
            services,
            owners: reload.facets.iter().map(|facet| facet.owner).collect(),
            gated: BTreeSet::new(),
            gated_facets: BTreeMap::new(),
        });
        Ok(reload)
    }

    /// Stops first access by old roots while allowing already admitted roots to reenter.
    /// Returns true only after their complete roots and owned tasks physically settle. Ending or
    /// cancelling a root does not release its lease while a callback is still running.
    pub fn gate(&self, ticket: Id, objects: &[Id]) -> Result<bool> {
        let mut inner = self.inner.borrow_mut();
        let pending = inner
            .pending
            .as_ref()
            .filter(|p| p.reload.id == ticket)
            .ok_or_else(|| conflict(Reason::ReloadTicket))?;
        if objects.iter().any(|id| !pending.objects.contains(id)) {
            return Err(Error::invalid(Reason::GateObject, "object is not affected"));
        }
        for id in objects {
            let roots = if inner.objects[id].gate.is_none() {
                Some(inner.object_roots.members(id).collect())
            } else {
                None
            };
            let object = inner
                .objects
                .get_mut(id)
                .ok_or_else(|| gone(Reason::Object))?;
            if object.gate.is_none() {
                object.gate = roots;
            }
        }
        inner
            .pending
            .as_mut()
            .expect("pending exists")
            .gated
            .extend(objects);
        Ok(objects.iter().all(|id| !inner.gate_object_busy(*id)))
    }

    /// Adopts declarations and private migrated states in one synchronous step.
    ///
    /// # Guarantees
    /// All refusal checks precede adoption. Failure preserves installed declarations and state.
    /// Adapter I/O, resource handoffs and other external effects are outside this guarantee.
    ///
    /// # Errors
    /// Refuses a foreign ticket, busy gates, missing gates, stale revisions or invalid migrations.
    pub fn commit(&self, ticket: Id, migrations: Vec<Migration>) -> Result<Version> {
        let mut inner = self.inner.borrow_mut();
        inner.open()?;
        let pending = inner
            .pending
            .as_ref()
            .filter(|p| p.reload.id == ticket)
            .ok_or_else(|| conflict(Reason::ReloadTicket))?;
        if pending
            .gated_facets
            .keys()
            .any(|owner| inner.facet_busy(*owner))
        {
            return Err(conflict(Reason::GateBusy));
        }
        if pending.gated.iter().any(|id| inner.gate_object_busy(*id)) {
            return Err(conflict(Reason::GateBusy));
        }
        // Claims may arrive while adapters open resources. Recheck them before adoption.
        for reference in pending
            .reload
            .objects
            .iter()
            .flat_map(|id| inner.object_references.members(id))
            .map(|id| &inner.references[&id])
            .filter(|reference| !reference.released)
        {
            let Some(object) = inner
                .objects
                .get(&reference.object)
                .filter(|object| !object.disposing)
            else {
                continue;
            };
            let (_, service, api_version) = pending
                .services
                .get(&object.address.service)
                .ok_or_else(|| conflict(Reason::LiveConsumer))?;
            if (reference.expected_api != 0 && reference.expected_api != *api_version)
                || !reference.methods.is_subset(&service.methods)
            {
                return Err(conflict(Reason::LiveConsumer));
            }
        }
        let mut migrated = BTreeSet::new();
        for migration in &migrations {
            if !migrated.insert(migration.object) || !pending.gated.contains(&migration.object) {
                return Err(conflict(Reason::MigrationGate));
            }
            let state = inner
                .objects
                .get(&migration.object)
                .ok_or_else(|| gone(Reason::Object))?
                .state
                .unwrap_or(Snapshot {
                    value: 0,
                    revision: 0,
                    version: 0,
                });
            if state.revision != migration.expected_revision {
                return Err(conflict(Reason::StateRevision));
            }
            if migration.value == 0 || migration.state_version <= state.version {
                return Err(Error::invalid(
                    Reason::StateVersion,
                    "migration must advance the state version",
                ));
            }
            if state.revision == u64::MAX {
                return Err(exhausted());
            }
        }
        let pending = inner.pending.take().expect("pending exists");
        for facet in &pending.reload.facets {
            inner.facets.insert(facet.id, facet.clone());
        }
        let previous_version = inner.version;
        inner.version = pending.reload.code_version;
        let version = inner.version;
        for owner in pending.gated_facets.keys() {
            inner.facet_min.insert(*owner, version);
        }
        for id in &pending.gated {
            inner.objects.get_mut(id).expect("gated object").min_version = version;
        }
        let services = pending
            .services
            .into_iter()
            .map(|(name, (facet, service, _))| (name, (facet, service)))
            .collect();
        for id in pending.current.values() {
            inner.facet_versions.insert(*id, version.into());
        }
        inner.services.insert(version, services);
        inner.current_owners = pending
            .current
            .values()
            .map(|id| inner.facets[id].owner)
            .collect();
        inner.versions.insert(version, pending.current);
        for id in &pending.reload.objects {
            let old = inner.objects[id].facet;
            let name = inner.facets[&old].declaration.name.clone();
            let replacement = inner.current().get(&name).copied();
            let service_name = inner.objects[id].address.service.clone();
            let service = replacement.and_then(|facet| {
                inner.facets[&facet]
                    .declaration
                    .services
                    .iter()
                    .find(|s| s.name == service_name)
                    .cloned()
            });
            if let (Some(facet), Some(_)) = (replacement, &service) {
                inner.facet_objects.remove(&old, *id);
                inner.facet_objects.insert(facet, *id);
            }
            let object = inner.objects.get_mut(id).expect("object exists");
            object.gate = None;
            object.preparing = false;
            if let (Some(facet), Some(service)) = (replacement, service) {
                object.facet = facet;
                object.pinned = service.lifetime == Lifetime::Host;
                object.linger_ms = service.linger_ms;
            } else {
                object.pinned = false;
                object.phase = Status::Gone;
            }
        }
        for migration in migrations {
            let object = inner
                .objects
                .get_mut(&migration.object)
                .expect("checked object");
            let previous = object.state;
            let revision = previous.map_or(0, |state| state.revision + 1);
            object.state = Some(Snapshot {
                value: migration.value,
                revision,
                version: migration.state_version,
            });
            object.min_version = version;
            if let Some(previous) = previous.filter(|previous| previous.value != migration.value) {
                inner.actions.push_back(Action::DropValue(previous.value));
            }
            inner.actions.push_back(Action::StateChanged {
                object: migration.object,
                revision,
            });
        }
        for id in pending.reload.objects {
            inner.maybe_dispose(id);
        }
        inner.collect_version(previous_version);
        Ok(inner.version)
    }

    /// Abandons preparation and reopens its gates. Candidate values remain adapter-owned.
    pub fn abort(&self, ticket: Id) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        if inner.pending.as_ref().is_none_or(|p| p.reload.id != ticket) {
            return Err(conflict(Reason::ReloadTicket));
        }
        for id in inner.pending.take().expect("pending exists").reload.objects {
            if let Some(object) = inner.objects.get_mut(&id) {
                object.gate = None;
                object.preparing = false;
            }
            inner.maybe_dispose(id);
        }
        Ok(())
    }

    /// Drains an exclusive facet-owned resource, including hook invocations without an object.
    pub fn gate_facets(&self, ticket: Id, owners: &[Id]) -> Result<bool> {
        let mut inner = self.inner.borrow_mut();
        let pending = inner
            .pending
            .as_ref()
            .filter(|pending| pending.reload.id == ticket)
            .ok_or_else(|| conflict(Reason::ReloadTicket))?;
        if owners.iter().any(|owner| {
            !pending
                .reload
                .facets
                .iter()
                .any(|facet| facet.owner == *owner)
        }) {
            return Err(Error::invalid(Reason::GateFacet, "facet is not affected"));
        }
        for owner in owners {
            if inner
                .pending
                .as_ref()
                .expect("pending")
                .gated_facets
                .contains_key(owner)
            {
                continue;
            }
            let roots = inner.facet_roots.members(owner).collect();
            inner
                .pending
                .as_mut()
                .expect("checked pending")
                .gated_facets
                .entry(*owner)
                .or_insert(roots);
        }
        Ok(owners.iter().all(|owner| !inner.facet_busy(*owner)))
    }

    pub fn installed_facets(&self) -> Vec<InstalledFacet> {
        let inner = self.inner.borrow();
        inner
            .current()
            .values()
            .map(|id| inner.facets[id].clone())
            .collect()
    }

    pub fn facet(&self, id: Id) -> Result<InstalledFacet> {
        self.inner
            .borrow()
            .facets
            .get(&id)
            .cloned()
            .ok_or_else(|| gone(Reason::Facet))
    }

    /// Resolves one current declaration without enumerating unrelated installed facets.
    pub fn facet_named(&self, name: &str) -> Result<InstalledFacet> {
        let inner = self.inner.borrow();
        let id = inner
            .current()
            .get(name)
            .ok_or_else(|| gone(Reason::Facet))?;
        Ok(inner.facets[id].clone())
    }

    pub fn version(&self) -> Version {
        self.inner.borrow().version
    }
}
