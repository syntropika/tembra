//! Adopt a prepared version or discard its candidate.

use super::*;

impl Host {
    /// Adopts declarations and private migrated states in one synchronous step.
    ///
    /// # Guarantees
    /// All refusal checks precede adoption. Failure preserves installed declarations and state.
    /// Compatible services keep their object identities and state when their provider orb changes.
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
            .gated_orbs
            .keys()
            .any(|owner| inner.orb_busy(*owner))
        {
            return Err(conflict(Reason::GateBusy));
        }
        if pending.gated.iter().any(|id| inner.gate_object_busy(*id)) {
            return Err(conflict(Reason::GateBusy));
        }
        // Claims may arrive while adapters open resources. Recheck them before adoption.
        if inner
            .consumer_mismatch(&pending.reload.objects, &pending.services)
            .is_some()
        {
            return Err(conflict(Reason::LiveConsumer));
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
                .unwrap_or_default();
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
        for orb in &pending.reload.orbs {
            inner.orbs.insert(orb.id, orb.clone());
        }
        let previous_version = inner.version;
        inner.version = pending.reload.code_version;
        let version = inner.version;
        for owner in pending.gated_orbs.keys() {
            inner.orb_min.insert(*owner, version);
        }
        for id in &pending.gated {
            inner.objects.get_mut(id).expect("gated object").min_version = version;
        }
        let services = pending
            .services
            .into_iter()
            .map(|(name, (orb, service, _))| (name, (orb, service)))
            .collect();
        for id in pending.current.values() {
            inner.orb_versions.insert(*id, version.into());
        }
        inner.services.insert(version, services);
        inner.current_owners = pending
            .current
            .values()
            .map(|id| inner.orbs[id].owner)
            .collect();
        inner.versions.insert(version, pending.current);
        for id in &pending.reload.objects {
            let old = inner.objects[id].orb;
            let replacement = inner.services[&version]
                .get(&inner.objects[id].address.service)
                .map(|(orb, service)| (*orb, service.lifetime, service.linger_ms));
            if let Some((orb, _, _)) = replacement {
                inner.orb_objects.remove(&old, *id);
                inner.orb_objects.insert(orb, *id);
            }
            let object = inner.objects.get_mut(id).expect("object exists");
            object.gate = None;
            object.preparing = false;
            if let Some((orb, lifetime, linger_ms)) = replacement {
                object.orb = orb;
                object.pinned = lifetime == Lifetime::Host;
                object.linger_ms = linger_ms;
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
}
