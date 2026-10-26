//! Installed and prepared facet lookup.

use super::*;

mod commit;
mod gate;
mod prepare;

impl Host {
    /// Describe the facets in the current implementation version.
    pub fn installed_facets(&self) -> Vec<InstalledFacet> {
        let inner = self.inner.borrow();
        inner
            .current()
            .values()
            .map(|id| inner.facets[id].clone())
            .collect()
    }

    /// Read metadata for a retained facet identity.
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

    /// Resolves a declaration in an isolated candidate without changing installed lookups.
    ///
    /// # Errors
    /// Refuses stale or foreign tickets and facet names absent from the candidate.
    pub fn prepared_facet(&self, ticket: Id, name: &str) -> Result<InstalledFacet> {
        let inner = self.inner.borrow();
        let pending = inner
            .pending
            .as_ref()
            .filter(|p| p.reload.id == ticket)
            .ok_or_else(|| conflict(Reason::ReloadTicket))?;
        let id = pending
            .current
            .get(name)
            .ok_or_else(|| gone(Reason::Facet))?;
        Ok(pending
            .facet_indices
            .get(id)
            .map(|index| &pending.reload.facets[*index])
            .or_else(|| inner.facets.get(id))
            .expect("candidate facet exists")
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
