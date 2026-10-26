//! Prepare isolated orb candidates.

use super::*;

impl Host {
    /// Check a replacement without changing installed state.
    /// Reject concurrent preparation and incompatible declarations.
    pub fn prepare(&self, declarations: Vec<Orb>, remove: Vec<String>) -> Result<Reload> {
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
        let mut orbs = Vec::new();
        let mut next = inner.next;
        for declaration in declarations {
            validate_orb(&declaration)?;
            if !names.insert(declaration.name.clone()) {
                return Err(Error::invalid(Reason::Duplicate, &declaration.name));
            }
            let id = reserve(&mut next)?;
            let owner = inner
                .current()
                .get(&declaration.name)
                .map(|id| inner.orbs[id].owner)
                .unwrap_or(id);
            current.insert(declaration.name.clone(), id);
            orbs.push(InstalledOrb {
                id,
                owner,
                code_version: version,
                declaration,
            });
        }
        let orb_indices: BTreeMap<_, _> = orbs.iter().enumerate().map(|(i, f)| (f.id, i)).collect();
        let lookup = |id: &Id| {
            orb_indices
                .get(id)
                .map(|index| &orbs[*index])
                .or_else(|| inner.orbs.get(id))
        };
        let mut services = BTreeMap::new();
        for id in current.values() {
            let orb = lookup(id).expect("prepared orb exists");
            for service in &orb.declaration.services {
                if services
                    .insert(
                        service.name.clone(),
                        (orb.id, service.clone(), orb.declaration.api_version),
                    )
                    .is_some()
                {
                    return Err(Error::invalid(Reason::DuplicateService, &service.name));
                }
            }
        }
        for id in current.values() {
            let orb = lookup(id).expect("prepared orb exists");
            for requirement in &orb.declaration.requires {
                let provider = current
                    .get(&requirement.orb)
                    .and_then(lookup)
                    .ok_or_else(|| {
                        Error::new(Code::Unavailable, Reason::Dependency, &requirement.orb)
                    })?;
                if provider.declaration.api_version != requirement.api_version {
                    return Err(conflict(Reason::ApiVersion));
                }
                for (name, methods) in &requirement.methods {
                    let (_, service, _) = services
                        .get(name)
                        .filter(|(orb, _, _)| *orb == provider.id)
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
            .flat_map(|orb| inner.orb_objects.members(orb))
            .filter(|id| !inner.objects[id].disposing && inner.objects[id].phase != Status::Gone)
            .collect();
        if let Some(reason) = inner.consumer_mismatch(&objects, &services) {
            return Err(conflict(reason));
        }
        if objects
            .iter()
            .any(|id| inner.objects[id].phase == Status::Loading)
        {
            return Err(conflict(Reason::ActivationBusy));
        }
        let ticket = reserve(&mut next)?;
        let mut reload = Reload {
            id: ticket,
            code_version: version,
            orbs,
            objects,
            providers: BTreeMap::new(),
        };
        inner.next = next;
        for object in &reload.objects {
            let object = inner.objects.get_mut(object).expect("checked object");
            object.preparing = true;
            let name = &object.address.service;
            if !reload.providers.contains_key(name)
                && let Some((orb, _, _)) = services.get(name)
            {
                reload.providers.insert(name.clone(), *orb);
            }
        }
        inner.pending = Some(Pending {
            reload: reload.clone(),
            objects: reload.objects.iter().copied().collect(),
            current,
            orb_indices,
            services,
            owners: reload.orbs.iter().map(|orb| orb.owner).collect(),
            gated: BTreeSet::new(),
            gated_orbs: BTreeMap::new(),
        });
        Ok(reload)
    }
}
