use super::*;
use crate::Reason;

impl Inner {
    /// Resolve the orb revision whose code declares resources for this owner.
    ///
    /// While a replacement is pending, owners and objects under replacement resolve to the
    /// candidate revision, matching the candidate code version assigned to new resources.
    fn declaring_revision(&self, owner: Owner) -> Result<Option<&InstalledOrb>> {
        let pending = self.pending.as_ref();
        let candidate = |id: Id| {
            pending.and_then(|pending| {
                pending
                    .orb_indices
                    .get(&id)
                    .map(|index| &pending.reload.orbs[*index])
            })
        };
        match owner {
            Owner::Host => Ok(None),
            Owner::Orb(owner) => {
                let revision = match pending.filter(|pending| pending.owners.contains(&owner)) {
                    Some(pending) => pending.reload.orbs.iter().find(|orb| orb.owner == owner),
                    None => self
                        .current()
                        .values()
                        .map(|id| &self.orbs[id])
                        .find(|orb| orb.owner == owner),
                };
                revision.map(Some).ok_or_else(|| gone(Reason::Orb))
            }
            Owner::Object(id) => {
                let object = self.objects.get(&id).ok_or_else(|| gone(Reason::Object))?;
                let revision = match pending.filter(|pending| pending.objects.contains(&id)) {
                    Some(pending) => pending
                        .services
                        .get(&object.address.service)
                        .map(|(orb, _, _)| *orb)
                        .ok_or_else(|| gone(Reason::Orb))?,
                    None => object.orb,
                };
                candidate(revision)
                    .or_else(|| self.orbs.get(&revision))
                    .map(Some)
                    .ok_or_else(|| gone(Reason::Orb))
            }
        }
    }

    /// Derive the execution realm: zero for host and native code, otherwise the isolated
    /// revision identity, so reloaded executor revisions never share values across heaps.
    fn resource_realm(&self, owner: Owner) -> Result<Id> {
        Ok(self
            .declaring_revision(owner)?
            .filter(|revision| revision.declaration.isolated)
            .map_or(0, |revision| revision.id))
    }
}

impl Host {
    /// Resolve a named resource under its declaration owner in the kernel-derived realm.
    pub fn resource(
        &self,
        owner: Owner,
        name: String,
        version: Version,
        exclusive: bool,
        dependencies: Vec<Id>,
    ) -> Result<Resource> {
        let realm = {
            let inner = self.inner.borrow();
            inner.owner(owner)?;
            inner.resource_realm(owner)?
        };
        self.declare_resource(owner, name, version, exclusive, dependencies, realm)
    }

    /// Reinstate an aborted replacement's previous resource in its original realm and version.
    pub(crate) fn restore_resource(
        &self,
        previous: &Resource,
        dependencies: Vec<Id>,
    ) -> Result<Resource> {
        let restored = self.declare_resource(
            previous.owner,
            previous.name.clone(),
            previous.version,
            previous.exclusive,
            dependencies,
            previous.realm,
        )?;
        let mut inner = self.inner.borrow_mut();
        let record = inner
            .resources
            .get_mut(&restored.id)
            .expect("restored resource");
        record.code_version = previous.code_version;
        Ok(record.clone())
    }

    /// Reuse a compatible record in the given realm or register a new one.
    fn declare_resource(
        &self,
        owner: Owner,
        name: String,
        version: Version,
        exclusive: bool,
        dependencies: Vec<Id>,
        realm: Id,
    ) -> Result<Resource> {
        let mut inner = self.inner.borrow_mut();
        inner.owner(owner)?;
        if name.is_empty() || version == 0 {
            return Err(Error::invalid(
                Reason::Resource,
                "resource name and version are required",
            ));
        }
        for dependency in &dependencies {
            if !inner.resources.get(dependency).is_some_and(|r| !r.closing) {
                return Err(Error::invalid(
                    Reason::ResourceDependency,
                    "dependency must be declared and live",
                ));
            }
        }
        let named = (owner, name.clone());
        if let Some(existing) = inner
            .resource_names
            .members(&named)
            .map(|id| &inner.resources[&id])
            .find(|r| {
                r.version == version
                    && r.realm == realm
                    && r.exclusive == exclusive
                    && r.dependencies == dependencies
                    && !r.closing
            })
        {
            return Ok(existing.clone());
        }
        if inner.pending.is_none() && inner.resource_names.contains(&named) {
            return Err(conflict(Reason::ResourceVersion));
        }
        let id = inner.id()?;
        let resource = Resource {
            id,
            owner,
            name,
            version,
            realm,
            code_version: inner
                .pending
                .as_ref()
                .map_or(inner.version, |pending| pending.reload.code_version),
            exclusive,
            dependencies,
            value: None,
            closing: false,
        };
        inner.owner_resources.insert(owner, id);
        inner.resource_names.insert(named, id);
        for dependency in &resource.dependencies {
            inner.dependents.insert(*dependency, id);
        }
        inner.resources.insert(id, resource.clone());
        Ok(resource)
    }

    /// Publish the value after its physical opener settles.
    pub fn resource_opened(&self, resource: Id, value: Id) -> Result<()> {
        if value == 0 {
            return Err(Error::invalid(
                Reason::Value,
                "zero is not a value identity",
            ));
        }
        let mut inner = self.inner.borrow_mut();
        let resource = inner
            .resources
            .get_mut(&resource)
            .ok_or_else(|| gone(Reason::Resource))?;
        if resource.closing || resource.value.is_some() {
            return Err(conflict(Reason::ResourceState));
        }
        let id = resource.id;
        resource.value = Some(value);
        inner.wake(WaitKey::Resource(id));
        Ok(())
    }

    /// Read an opened resource value or reject unavailable storage.
    pub fn resource_value(&self, resource: Id) -> Result<Id> {
        let inner = self.inner.borrow();
        let resource = inner
            .resources
            .get(&resource)
            .ok_or_else(|| gone(Reason::Resource))?;
        if resource.closing {
            return Err(gone(Reason::Resource));
        }
        resource
            .value
            .ok_or_else(|| Error::new(Code::Unavailable, Reason::NotReady, "resource is not open"))
    }

    /// Requests closure only after every resource dependent has closed.
    pub fn close_resource(&self, resource: Id) -> Result<bool> {
        self.close_resource_before(resource, None)
    }

    /// Retiring adapters may exclude successor invocations, which cannot use this resource.
    pub fn close_resource_before(&self, resource: Id, before: Option<Version>) -> Result<bool> {
        let mut inner = self.inner.borrow_mut();
        if !inner.resources.contains_key(&resource) {
            return Err(gone(Reason::Resource));
        }
        let owner = inner.resources[&resource].owner;
        if inner.owner_busy(owner, inner.resources[&resource].code_version, before) {
            return Ok(false);
        }
        if inner.dependents.contains(&resource) {
            return Ok(false);
        }
        let record = inner
            .resources
            .get_mut(&resource)
            .expect("checked resource");
        // A physical closer has exactly one owner, including across cleanup programs.
        if record.closing {
            return Ok(false);
        }
        record.closing = true;
        inner.wake(WaitKey::Resource(resource));
        inner.actions.push_back(Action::CloseResource(resource));
        Ok(true)
    }

    /// Remove the resource and wake its indexed dependents.
    pub fn resource_closed(&self, resource: Id) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        let record = inner
            .resources
            .get(&resource)
            .ok_or_else(|| gone(Reason::Resource))?;
        if !record.closing {
            return Err(conflict(Reason::ResourceState));
        }
        let record = inner.resources.remove(&resource).expect("checked resource");
        inner.owner_resources.remove(&record.owner, resource);
        inner
            .resource_names
            .remove(&(record.owner, record.name), resource);
        for dependency in record.dependencies {
            inner.dependents.remove(&dependency, resource);
            inner.wake(WaitKey::Resource(dependency));
        }
        inner.wake(WaitKey::Resource(resource));
        if let Some(value) = record.value {
            inner.actions.push_back(Action::DropValue(value));
        }
        Ok(())
    }
}
