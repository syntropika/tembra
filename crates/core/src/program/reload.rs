//! Candidate replacement and explicit commit or abort.

use super::*;

impl Host {
    /// Builds the shared reload policy after language factories have reported their declarations.
    /// The deadline starts at this host's most recent [`Host::tick`].
    pub fn reload_registered_scopes(&self, ticket: Id, deadline_ms: u64) -> Result<Id> {
        let inner = self.inner.borrow();
        let pending = inner
            .pending
            .as_ref()
            .filter(|pending| pending.reload.id == ticket)
            .ok_or_else(|| conflict(Reason::ReloadTicket))?;
        let mut pairs = BTreeMap::<Owner, Replacement>::new();
        let previous = inner
            .current()
            .iter()
            .filter(|(name, id)| pending.current.get(*name) != Some(*id))
            .map(|(_, id)| *id);
        for (orbs, next) in [
            (previous.collect::<Vec<_>>(), false),
            (pending.reload.orbs.iter().map(|orb| orb.id).collect(), true),
        ] {
            for orb in orbs {
                for id in inner.orb_scopes.members(&orb) {
                    let scope = &inner.scopes[&id];
                    if scope.closing {
                        continue;
                    }
                    if let Owner::Object(object) = scope.owner
                        && !pending.objects.contains(&object)
                    {
                        continue;
                    }
                    let pair = pairs.entry(scope.owner).or_insert(Replacement {
                        previous: None,
                        next: None,
                    });
                    let slot = if next {
                        &mut pair.next
                    } else {
                        &mut pair.previous
                    };
                    if slot.replace(id).is_some() {
                        return Err(conflict(Reason::AmbiguousScope));
                    }
                }
            }
        }
        let pairs = pairs.into_values().collect();
        drop(inner);
        self.reload_scopes(ticket, pairs, deadline_ms)
    }

    /// Accepts explicit pairs for native integrations with independently declared scopes.
    /// The deadline starts at this host's most recent [`Host::tick`].
    pub fn reload_scopes(
        &self,
        ticket: Id,
        replacements: Vec<Replacement>,
        deadline_ms: u64,
    ) -> Result<Id> {
        let mut inner = self.inner.borrow_mut();
        inner.open()?;
        let reload = &inner
            .pending
            .as_ref()
            .filter(|pending| pending.reload.id == ticket)
            .ok_or_else(|| conflict(Reason::ReloadTicket))?
            .reload;
        if deadline_ms == 0 {
            return Err(Error::invalid(
                Reason::Deadline,
                "deadline must be positive",
            ));
        }
        let mut program = Program::new(
            replacements.iter().filter_map(|pair| pair.next).collect(),
            Phase::Stop,
        );
        program.ticket = Some(ticket);
        program.deadline = Some(inner.now.saturating_add(deadline_ms));
        for pair in &replacements {
            let previous = pair
                .previous
                .map(|id| inner.scopes.get(&id).ok_or_else(|| gone(Reason::Scope)))
                .transpose()?;
            let next = pair
                .next
                .map(|id| inner.scopes.get(&id).ok_or_else(|| gone(Reason::Scope)))
                .transpose()?;
            if let (Some(previous), Some(next)) = (previous, next)
                && previous.owner != next.owner
            {
                return Err(conflict(Reason::ScopeOwner));
            }
            if let Some(previous) = previous {
                program.retained.extend(&previous.resources);
            }
            if let Some(next) = next {
                if !reload.orbs.iter().any(|orb| orb.id == next.orb) {
                    return Err(conflict(Reason::ScopeOrb));
                }
                if let (Owner::Object(object), Some(version)) = (next.owner, next.state_version) {
                    let snapshot = inner
                        .objects
                        .get(&object)
                        .ok_or_else(|| gone(Reason::Object))?
                        .state
                        .unwrap_or_default();
                    if version < snapshot.version {
                        return Err(conflict(Reason::StateVersion));
                    }
                    if version > snapshot.version {
                        if snapshot.version != 0 && !next.migrate {
                            return Err(Error::invalid(
                                Reason::MigrationRequired,
                                "state version changes require migration",
                            ));
                        }
                        program.objects.push(object);
                    }
                }
                if let Some(previous) = previous {
                    for id in &next.resources {
                        let resource = &inner.resources[id];
                        if let Some(old) = previous
                            .resources
                            .iter()
                            .filter_map(|id| inner.resources.get(id))
                            .find(|old| {
                                old.name == resource.name
                                    && old.id != resource.id
                                    && (old.exclusive || resource.exclusive)
                            })
                        {
                            program.exclusive.push(old.clone());
                            match next.owner {
                                Owner::Object(id) => program.objects.push(id),
                                Owner::Orb(id) => program.orbs.push(id),
                                Owner::Host => return Err(conflict(Reason::HostResourceReload)),
                            }
                        }
                    }
                }
            }
        }
        // An exclusive handoff also drains its dependents. Retain their declarations for
        // rollback, and refuse a handoff that would close a scope outside this replacement.
        let previous_scopes: BTreeSet<_> = replacements
            .iter()
            .filter_map(|pair| pair.previous)
            .collect();
        let previous_resources: BTreeSet<_> = previous_scopes
            .iter()
            .flat_map(|scope| inner.scopes[scope].resources.iter().copied())
            .collect();
        let mut closure = BTreeMap::new();
        let mut pending = std::mem::take(&mut program.exclusive);
        while let Some(resource) = pending.pop() {
            if closure.contains_key(&resource.id) {
                continue;
            }
            if inner
                .resource_scopes
                .members(&resource.id)
                .any(|scope| !previous_scopes.contains(&scope))
            {
                return Err(conflict(Reason::ExclusiveDependent));
            }
            match resource.owner {
                Owner::Object(id) => program.objects.push(id),
                Owner::Orb(id) => program.orbs.push(id),
                Owner::Host => return Err(conflict(Reason::HostResourceReload)),
            }
            for id in inner.dependents.members(&resource.id) {
                if !previous_resources.contains(&id) {
                    return Err(conflict(Reason::ExclusiveDependent));
                }
                pending.push(inner.resources[&id].clone());
            }
            closure.insert(resource.id, resource);
        }
        program.exclusive = closure.into_values().collect();
        program.objects.sort_unstable();
        program.objects.dedup();
        program.orbs.sort_unstable();
        program.orbs.dedup();
        program.previous_scopes = replacements
            .iter()
            .filter_map(|pair| pair.next.map(|next| (next, pair.previous)))
            .collect();
        program.replacements = replacements;
        let id = inner.id()?;
        for pair in &program.replacements {
            for scope in [pair.previous, pair.next].into_iter().flatten() {
                inner.scope_programs.insert(scope, id);
            }
        }
        inner
            .program_deadlines
            .entry(program.deadline.expect("reload deadline"))
            .or_default()
            .insert(id);
        inner.programs.insert(id, program);
        Ok(id)
    }

    /// Admit publication of a fully prepared replacement.
    pub fn commit_program(&self, id: Id) -> Result<()> {
        let (ticket, migrations) = {
            let inner = self.inner.borrow();
            let program = inner
                .programs
                .get(&id)
                .ok_or_else(|| gone(Reason::Program))?;
            if program.phase != Phase::Prepared {
                return Err(conflict(Reason::ProgramPhase));
            }
            (
                program
                    .ticket
                    .ok_or_else(|| conflict(Reason::ReloadTicket))?,
                program.migrations.clone(),
            )
        };
        {
            let inner = self.inner.borrow();
            for (scope, expected) in &inner.programs[&id].expected {
                if let Owner::Object(object) = inner.scopes[scope].owner {
                    let current = inner.objects[&object].state;
                    if current.unwrap_or_default() != *expected {
                        return Err(conflict(Reason::StateRevision));
                    }
                }
            }
        }
        self.commit(ticket, migrations)?;
        let mut inner = self.inner.borrow_mut();
        let program = inner.programs.get_mut(&id).expect("checked program");
        let deadline = program.deadline.take();
        program.committed = true;
        program.phase = Phase::Adopt;
        program.running = true;
        inner.remove_deadline(deadline, id);
        inner.ready_programs.insert(id);
        Ok(())
    }

    /// Start recovery without claiming pending callbacks have stopped.
    pub fn abort_program(&self, id: Id) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        let program = inner
            .programs
            .get_mut(&id)
            .ok_or_else(|| gone(Reason::Program))?;
        if matches!(
            program.phase,
            Phase::Adopt | Phase::Start | Phase::Drop | Phase::Complete
        ) {
            return Err(conflict(Reason::AlreadyCommitted));
        }
        program.error.get_or_insert_with(|| {
            Error::new(Code::Cancelled, Reason::ReloadAborted, "reload was aborted")
        });
        program.running = true;
        inner.ready_programs.insert(id);
        Ok(())
    }
}
