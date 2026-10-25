use super::*;

impl Inner {
    // A released claim cannot retain dependency edges after its record is forgotten.
    fn unlink_reference(&mut self, reference: Id, target: Id) {
        self.object_dependents.remove(&target, reference);
        for scope in self.reference_scopes.members(&reference) {
            if !self.scopes[&scope].dependencies.iter().any(|other| {
                *other != reference
                    && self
                        .references
                        .get(other)
                        .is_some_and(|claim| !claim.released && claim.object == target)
            }) {
                self.scope_dependencies.remove(&target, scope);
            }
        }
    }
}

impl Host {
    pub fn object_address(&self, object: Id) -> Result<Address> {
        self.inner
            .borrow()
            .objects
            .get(&object)
            .map(|o| o.address.clone())
            .ok_or_else(|| gone("object"))
    }

    /// Acquires an independent claim. Cloning its numeric identity does not create another claim.
    /// A disposing object is never resurrected or overlapped at the same address.
    pub fn acquire(
        &self,
        address: Address,
        api_version: Version,
        methods: BTreeSet<String>,
    ) -> Result<Acquisition> {
        let mut inner = self.inner.borrow_mut();
        inner.open()?;
        if address.service.is_empty() || address.key.is_empty() {
            return Err(Error::invalid(
                "address",
                "service and key must be nonempty",
            ));
        }
        let (facet, service) = inner.find_service(&address.service, inner.version)?;
        let declaration = &inner.facets[&facet];
        if api_version != 0 && api_version != declaration.declaration.api_version {
            return Err(conflict("api_version"));
        }
        if !methods.is_subset(&service.methods) {
            return Err(conflict("missing_method"));
        }
        let code_version = inner.version;
        let lifetime = service.lifetime;
        let linger_ms = service.linger_ms;
        if service.lifetime == Lifetime::Host
            && (address.key != ["main"] || address.principal != "host")
        {
            return Err(Error::invalid(
                "host_address",
                "host-lifetime objects use key main and the host principal",
            ));
        }
        let existing = inner.addresses.get(&address).copied();
        if existing.is_none()
            && inner.pending.as_ref().is_some_and(|pending| {
                pending.current.get(&inner.facets[&facet].declaration.name) != Some(&facet)
            })
        {
            return Err(conflict("reload_preparing"));
        }
        if existing.is_some_and(|id| {
            inner.objects[&id].disposing || inner.objects[&id].phase == Status::Gone
        }) {
            return Err(Error::new(
                Code::Unavailable,
                "disposing",
                "the previous instance is still closing",
            ));
        }
        // Reserve all identities before changing the object table.
        let reference = inner.id()?;
        let object = match existing {
            Some(id) => id,
            None => inner.id()?,
        };
        if existing.is_none() {
            inner.facet_objects.insert(facet, object);
            inner.addresses.insert(address.clone(), object);
            inner.objects.insert(
                object,
                Object {
                    address,
                    facet,
                    phase: Status::Loading,
                    references: 0,
                    calls: 0,
                    tasks: 0,
                    pinned: lifetime == Lifetime::Host,
                    disposing: false,
                    preparing: false,
                    idle_at: None,
                    linger_ms,
                    gate: None,
                    min_version: 1,
                    state: None,
                    dependencies: BTreeSet::new(),
                },
            );
        }
        if let Some(since) = inner.objects[&object].idle_at {
            let deadline = since.saturating_add(u64::from(inner.objects[&object].linger_ms));
            if let Some(ids) = inner.idle.get_mut(&deadline) {
                ids.remove(&object);
                if ids.is_empty() {
                    inner.idle.remove(&deadline);
                }
            }
        }
        let record = inner.objects.get_mut(&object).expect("object exists");
        record.references += 1;
        record.idle_at = None;
        inner.object_references.insert(object, reference);
        inner.references.insert(
            reference,
            Reference {
                object,
                released: false,
                expected_api: api_version,
                methods,
                #[cfg(feature = "views")]
                allowed: None,
            },
        );
        Ok(Acquisition {
            reference,
            object,
            facet,
            code_version,
            activate: existing.is_none(),
        })
    }

    pub fn ready(&self, object: Id) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        let record = inner
            .objects
            .get_mut(&object)
            .ok_or_else(|| gone("object"))?;
        if record.phase != Status::Loading || record.disposing {
            return Err(gone("object"));
        }
        record.phase = Status::Live;
        inner.wake(WaitKey::Object(object));
        inner.maybe_dispose(object);
        Ok(())
    }

    /// Retains an object-owned dependency and rejects local dependency cycles before activation.
    pub fn dependency(&self, object: Id, reference: Id) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        let target = inner
            .references
            .get(&reference)
            .filter(|r| !r.released)
            .ok_or_else(|| gone("reference"))?
            .object;
        let mut pending = vec![target];
        let mut visited = BTreeSet::new();
        while let Some(id) = pending.pop() {
            if id == object {
                return Err(Error::invalid(
                    "dependency_cycle",
                    "object dependency graph must be acyclic",
                ));
            }
            if visited.insert(id)
                && let Some(record) = inner.objects.get(&id)
            {
                pending.extend(record.dependencies.iter().filter_map(|r| {
                    inner
                        .references
                        .get(r)
                        .filter(|r| !r.released)
                        .map(|r| r.object)
                }));
            }
        }
        let record = inner
            .objects
            .get_mut(&object)
            .ok_or_else(|| gone("object"))?;
        if record.disposing {
            return Err(gone("object"));
        }
        if record.dependencies.insert(reference) {
            inner.object_dependents.insert(target, reference);
        }
        Ok(())
    }

    pub fn fail(&self, object: Id) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        let record = inner
            .objects
            .get_mut(&object)
            .ok_or_else(|| gone("object"))?;
        record.phase = Status::Gone;
        record.pinned = false;
        inner.wake(WaitKey::Object(object));
        inner.maybe_dispose(object);
        Ok(())
    }

    pub fn detach_dependency(&self, object: Id, reference: Id) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        let record = inner
            .objects
            .get_mut(&object)
            .ok_or_else(|| gone("object"))?;
        record.dependencies.remove(&reference);
        if let Some(target) = inner
            .references
            .get(&reference)
            .map(|reference| reference.object)
        {
            inner.object_dependents.remove(&target, reference);
            inner.maybe_dispose(target);
        }
        Ok(())
    }

    pub fn status(&self, reference: Id) -> Result<Status> {
        let inner = self.inner.borrow();
        let record = inner
            .references
            .get(&reference)
            .ok_or_else(|| gone("reference"))?;
        if record.released {
            return Ok(Status::Released);
        }
        if inner.closing {
            return Ok(Status::Gone);
        }
        Ok(inner
            .objects
            .get(&record.object)
            .map_or(Status::Gone, |o| o.phase))
    }

    pub fn release(&self, reference: Id) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        let reference_id = reference;
        let reference = inner
            .references
            .get_mut(&reference)
            .ok_or_else(|| gone("reference"))?;
        if reference.released {
            return Ok(());
        }
        reference.released = true;
        let object = reference.object;
        inner.unlink_reference(reference_id, object);
        inner.wake(WaitKey::Reference(reference_id));
        if let Some(record) = inner.objects.get_mut(&object) {
            record.references -= 1;
        }
        inner.maybe_dispose(object);
        Ok(())
    }

    /// Releases a terminal reference record after the adapter has revoked every public handle.
    pub fn forget_reference(&self, reference: Id) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        if inner
            .references
            .get(&reference)
            .is_some_and(|r| !r.released)
        {
            return Err(conflict("live_reference"));
        }
        let target = inner
            .references
            .remove(&reference)
            .map(|claim| claim.object);
        inner.wake(WaitKey::Reference(reference));
        if let Some(target) = target {
            inner.unlink_reference(reference, target);
            inner.object_references.remove(&target, reference);
        }
        if let Some(target) = target {
            inner.maybe_dispose(target);
        }
        Ok(())
    }

    #[cfg(feature = "views")]
    pub fn restrict(&self, reference: Id, allowed: BTreeSet<String>) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        let reference = inner
            .references
            .get_mut(&reference)
            .ok_or_else(|| gone("reference"))?;
        if reference.released {
            return Err(gone("reference"));
        }
        reference.allowed = Some(allowed);
        Ok(())
    }
}
