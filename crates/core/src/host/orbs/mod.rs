//! Installed and prepared orb lookup.

use super::*;

mod commit;
mod gate;
mod prepare;

impl Host {
    /// Describe the orbs in the current implementation version.
    pub fn installed_orbs(&self) -> Vec<InstalledOrb> {
        let inner = self.inner.borrow();
        inner
            .current()
            .values()
            .map(|id| inner.orbs[id].clone())
            .collect()
    }

    /// Read metadata for a retained orb identity.
    pub fn orb(&self, id: Id) -> Result<InstalledOrb> {
        self.inner
            .borrow()
            .orbs
            .get(&id)
            .cloned()
            .ok_or_else(|| gone(Reason::Orb))
    }

    /// Resolves one current declaration without enumerating unrelated installed orbs.
    pub fn orb_named(&self, name: &str) -> Result<InstalledOrb> {
        let inner = self.inner.borrow();
        let id = inner.current().get(name).ok_or_else(|| gone(Reason::Orb))?;
        Ok(inner.orbs[id].clone())
    }

    /// Resolves a declaration in an isolated candidate without changing installed lookups.
    ///
    /// # Errors
    /// Refuses stale or foreign tickets and orb names absent from the candidate.
    pub fn prepared_orb(&self, ticket: Id, name: &str) -> Result<InstalledOrb> {
        let inner = self.inner.borrow();
        let pending = inner
            .pending
            .as_ref()
            .filter(|p| p.reload.id == ticket)
            .ok_or_else(|| conflict(Reason::ReloadTicket))?;
        let id = pending.current.get(name).ok_or_else(|| gone(Reason::Orb))?;
        Ok(pending
            .orb_indices
            .get(id)
            .map(|index| &pending.reload.orbs[*index])
            .or_else(|| inner.orbs.get(id))
            .expect("candidate orb exists")
            .clone())
    }

    /// Read the published implementation version.
    pub fn version(&self) -> Version {
        self.inner.borrow().version
    }
}

impl Inner {
    /// Check live claims against candidate services without scanning unrelated objects.
    fn consumer_mismatch(
        &self,
        objects: &[Id],
        services: &BTreeMap<String, (Id, Service, Version)>,
    ) -> Option<Reason> {
        for reference in objects
            .iter()
            .flat_map(|id| self.object_references.members(id))
            .map(|id| &self.references[&id])
            .filter(|reference| !reference.released)
        {
            let Some(object) = self
                .objects
                .get(&reference.object)
                .filter(|object| !object.disposing)
            else {
                continue;
            };
            let Some((_, service, api_version)) = services.get(&object.address.service) else {
                return Some(Reason::LiveConsumer);
            };
            if reference.expected_api != 0 && reference.expected_api != *api_version {
                return Some(Reason::ApiVersion);
            }
            if !reference.methods.is_subset(&service.methods) {
                return Some(Reason::MissingMethod);
            }
        }
        None
    }
}
