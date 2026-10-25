//! Language-neutral lifecycle programs. Adapters execute effects; this module decides their order.
use crate::Reason;
use crate::{
    Code, Error, Host, Id, Migration, Owner, Resource, Result, Snapshot, Version,
    host::{conflict, gone},
};
use std::collections::{BTreeMap, BTreeSet};
#[cfg(test)]
mod tests;

#[derive(Clone, Debug)]
pub struct Scope {
    pub id: Id,
    pub owner: Owner,
    pub facet: Id,
    pub parents: Vec<Id>,
    pub dependencies: Vec<Id>,
    pub resources: Vec<Id>,
    pub state_version: Option<Version>,
    pub migrate: bool,
    pub active: bool,
    pub closing: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Replacement {
    pub previous: Option<Id>,
    pub next: Option<Id>,
}

/// These are execution requests, not policy hooks. Every platform runs the same program.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Effect {
    Stop {
        scope: Id,
        keep_listeners: bool,
    },
    Open {
        resource: Id,
    },
    Close {
        resource: Id,
    },
    Restore {
        previous: Id,
        resource: Id,
    },
    Prepare {
        scope: Id,
        previous: Option<Id>,
    },
    Validate {
        scope: Id,
        value: Id,
    },
    Initialize {
        scope: Id,
    },
    Migrate {
        scope: Id,
        value: Option<Id>,
        version: Version,
    },
    Adopt {
        replacements: Vec<Replacement>,
    },
    Start {
        scope: Id,
    },
    Drop {
        scope: Id,
    },
}
#[derive(Clone, Debug)]
pub enum Step {
    Execute(Vec<Effect>),
    Wait { deadline: Option<u64> },
    Prepared,
    Complete,
    Failed(Error),
}
#[derive(Clone, Debug)]
pub struct Completion {
    pub value: Option<Id>,
    pub error: Option<Error>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Phase {
    Stop,
    Gate,
    CloseExclusive,
    Open,
    Prepare,
    State,
    Prepared,
    Adopt,
    Start,
    Drop,
    Complete,
    AbortStop,
    AbortClose,
    Restore,
    Resume,
    AbortDrop,
    Failed,
}
/// Only a change to one of these identities can unblock a waiting program.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) enum WaitKey {
    Owner(Owner),
    Scope(Id),
    Object(Id),
    Reference(Id),
    HostScopes,
    Resource(Id),
    Program(Id),
}
#[derive(Clone, Debug)]
pub(crate) struct Program {
    pub(crate) ticket: Option<Id>,
    pub(crate) replacements: Vec<Replacement>,
    previous_scopes: BTreeMap<Id, Option<Id>>,
    scope_set: BTreeSet<Id>,
    scopes: Vec<Id>,
    object: Option<Id>,
    pub(crate) activation_scope: Option<Id>,
    activation_owner: Option<Owner>,
    phase: Phase,
    pending: Vec<Effect>,
    migrations: Vec<Migration>,
    expected: BTreeMap<Id, Snapshot>,
    objects: Vec<Id>,
    facets: Vec<Id>,
    exclusive: Vec<Resource>,
    retained: BTreeSet<Id>,
    pub(crate) deadline: Option<u64>,
    error: Option<Error>,
    restored: BTreeMap<Id, Id>,
    committed: bool,
    running: bool,
    cancelled_effects: bool,
    pub(crate) waiting: BTreeSet<WaitKey>,
}

impl Host {
    pub fn register_scope(&self, scope: Scope) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        inner.open()?;
        inner.owner(scope.owner)?;
        if scope.id == 0
            || inner.scopes.contains_key(&scope.id)
            || scope
                .parents
                .iter()
                .any(|id| !inner.scopes.contains_key(id))
        {
            return Err(Error::invalid(
                Reason::Scope,
                "scope identity and parents must be valid",
            ));
        }
        if scope.resources.iter().any(|id| {
            inner
                .resources
                .get(id)
                .is_none_or(|resource| resource.owner != scope.owner)
        }) {
            return Err(Error::invalid(
                Reason::ScopeResource,
                "scope must own its declared resources",
            ));
        }
        if !matches!(scope.owner, Owner::Object(_)) {
            for reference in &scope.dependencies {
                if let Some(target) = inner.references.get(reference).map(|claim| claim.object) {
                    inner.scope_dependencies.insert(target, scope.id);
                }
            }
        }
        inner.owner_scopes.insert(scope.owner, scope.id);
        if scope.owner != Owner::Host {
            inner.non_host_scopes += 1;
        }
        for reference in &scope.dependencies {
            inner.reference_scopes.insert(*reference, scope.id);
        }
        inner.facet_scopes.insert(scope.facet, scope.id);
        for parent in &scope.parents {
            inner.scope_children.insert(*parent, scope.id);
        }
        for resource in &scope.resources {
            inner.resource_scopes.insert(*resource, scope.id);
            if !scope.closing {
                inner.resource_claims.insert(*resource, scope.id);
            }
        }
        if let Some(version) = inner
            .facets
            .get(&scope.facet)
            .or_else(|| {
                inner.pending.as_ref().and_then(|pending| {
                    pending
                        .reload
                        .facets
                        .iter()
                        .find(|facet| facet.id == scope.facet)
                })
            })
            .map(|facet| facet.code_version)
        {
            inner.scope_code_versions.insert(scope.id, version);
        }
        if !scope.closing {
            inner.track_scope_version(&scope, true);
        }
        inner.scopes.insert(scope.id, scope);
        Ok(())
    }

    pub fn activate_scope(&self, scope: Id) -> Result<Id> {
        let mut inner = self.inner.borrow_mut();
        if !inner.scopes.contains_key(&scope) {
            return Err(gone(Reason::Scope));
        }
        let id = inner.id()?;
        if let Some(program) = inner.scope_activations.get(&scope) {
            return Ok(*program);
        }
        let mut program = Program::new(vec![scope], Phase::Open);
        program.activation_scope = Some(scope);
        program.activation_owner = Some(inner.scopes[&scope].owner);
        inner.scope_activations.insert(scope, id);
        inner.programs.insert(id, program);
        Ok(id)
    }

    pub fn cleanup_scopes(&self, scopes: Vec<Id>, object: Option<Id>) -> Result<Id> {
        let mut inner = self.inner.borrow_mut();
        if scopes.iter().any(|id| !inner.scopes.contains_key(id)) {
            return Err(gone(Reason::Scope));
        }
        inner.begin_cleanup(&scopes);
        let id = inner.id()?;
        let mut program = Program::new(scopes, Phase::Stop);
        program.object = object;
        inner.programs.insert(id, program);
        Ok(id)
    }

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
        for (facets, next) in [
            (previous.collect::<Vec<_>>(), false),
            (
                pending.reload.facets.iter().map(|facet| facet.id).collect(),
                true,
            ),
        ] {
            for facet in facets {
                for id in inner.facet_scopes.members(&facet) {
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
        let reload = inner
            .pending
            .as_ref()
            .filter(|pending| pending.reload.id == ticket)
            .ok_or_else(|| conflict(Reason::ReloadTicket))?
            .reload
            .clone();
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
                if !reload.facets.iter().any(|facet| facet.id == next.facet) {
                    return Err(conflict(Reason::ScopeFacet));
                }
                if let (Owner::Object(object), Some(version)) = (next.owner, next.state_version) {
                    let snapshot = inner
                        .objects
                        .get(&object)
                        .ok_or_else(|| gone(Reason::Object))?
                        .state
                        .unwrap_or(Snapshot {
                            value: 0,
                            revision: 0,
                            version: 0,
                        });
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
                                Owner::Facet(id) => program.facets.push(id),
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
                Owner::Facet(id) => program.facets.push(id),
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
        program.facets.sort_unstable();
        program.facets.dedup();
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

    /// Acknowledges only physical execution settlement. Observer cancellation is not completion.
    pub fn complete_step(&self, id: Id, results: Vec<Completion>) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        #[cfg(feature = "subscriptions")]
        let mut started = Vec::new();
        let crate::host::Inner {
            programs, scopes, ..
        } = &mut *inner;
        let program = programs.get_mut(&id).ok_or_else(|| gone(Reason::Program))?;
        if program.pending.len() != results.len() || results.is_empty() {
            return Err(conflict(Reason::EffectResults));
        }
        for (effect, result) in std::mem::take(&mut program.pending)
            .into_iter()
            .zip(results)
        {
            if let Some(error) = result.error {
                program.error.get_or_insert(error);
                continue;
            }
            #[cfg(feature = "subscriptions")]
            if let Effect::Start { scope } = effect {
                started.push(scope);
            }
            if let Effect::Migrate { scope, .. } | Effect::Initialize { scope } = effect {
                let value = result.value.ok_or_else(|| {
                    Error::invalid(
                        Reason::MigrationValue,
                        "migration must return a value identity",
                    )
                })?;
                let snapshot = program.expected[&scope];
                let declared = &scopes[&scope];
                let Owner::Object(object) = declared.owner else {
                    return Err(conflict(Reason::StateOwner));
                };
                program.migrations.push(Migration {
                    object,
                    expected_revision: snapshot.revision,
                    value,
                    state_version: declared.state_version.expect("state declaration"),
                });
            }
        }
        program.running = true;
        inner.ready_programs.insert(id);
        #[cfg(feature = "subscriptions")]
        {
            drop(inner);
            for scope in started {
                self.start_scope_listeners(scope);
            }
        }
        Ok(())
    }

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
                    if current.unwrap_or(Snapshot {
                        value: 0,
                        revision: 0,
                        version: 0,
                    }) != *expected
                    {
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

    pub fn next_step(&self, id: Id) -> Result<Step> {
        let mut inner = self.inner.borrow_mut();
        let mut program = inner
            .programs
            .remove(&id)
            .ok_or_else(|| gone(Reason::Program))?;
        for key in std::mem::take(&mut program.waiting) {
            inner.waiters.remove(&key, id);
        }
        let deadline = program.deadline;
        drop(inner);
        let result = self.advance(id, &mut program);
        if !matches!(&result, Ok(Step::Wait { .. })) {
            program.waiting.clear();
        }
        let mut inner = self.inner.borrow_mut();
        if deadline != program.deadline {
            inner.remove_deadline(deadline, id);
        }
        for key in &program.waiting {
            inner.waiters.insert(*key, id);
        }
        inner.programs.insert(id, program);
        result
    }

    pub fn forget_program(&self, id: Id) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        if inner
            .programs
            .get(&id)
            .is_some_and(|program| !matches!(program.phase, Phase::Complete | Phase::Failed))
        {
            return Err(conflict(Reason::ProgramRunning));
        }
        inner.remove_program(id);
        Ok(())
    }

    fn advance(&self, id: Id, program: &mut Program) -> Result<Step> {
        if !program.pending.is_empty() {
            return Err(conflict(Reason::EffectsRunning));
        }
        loop {
            if program
                .deadline
                .is_some_and(|deadline| self.inner.borrow().now >= deadline)
            {
                program
                    .error
                    .get_or_insert_with(|| conflict(Reason::ReloadDeadline));
                program.deadline = None;
            }
            if program.error.is_some()
                && !program.committed
                && !matches!(
                    program.phase,
                    Phase::AbortStop
                        | Phase::AbortClose
                        | Phase::Restore
                        | Phase::Resume
                        | Phase::AbortDrop
                        | Phase::Failed
                        | Phase::Complete
                        | Phase::Drop
                )
            {
                program.phase = Phase::AbortStop;
            }
            let mut effects = Vec::new();
            match program.phase {
                Phase::Stop => {
                    if program.ticket.is_none() && self.wait_for_reloads(id, program) {
                        return Ok(Step::Wait { deadline: None });
                    }
                    if program.ticket.is_some() {
                        effects.extend(program.replacements.iter().filter_map(|pair| {
                            pair.previous.map(|scope| Effect::Stop {
                                scope,
                                keep_listeners: pair.next.is_some(),
                            })
                        }));
                        program.phase = Phase::Gate;
                    } else {
                        effects.extend(program.scopes.iter().map(|scope| Effect::Stop {
                            scope: *scope,
                            keep_listeners: false,
                        }));
                        program.phase = Phase::AbortClose;
                    }
                }
                Phase::Gate => {
                    let ticket = program.ticket.expect("reload");
                    if !self.gate(ticket, &program.objects)?
                        || !self.gate_facets(ticket, &program.facets)?
                    {
                        program.waiting.extend(
                            program
                                .objects
                                .iter()
                                .map(|id| WaitKey::Owner(Owner::Object(*id))),
                        );
                        program.waiting.extend(
                            program
                                .facets
                                .iter()
                                .map(|id| WaitKey::Owner(Owner::Facet(*id))),
                        );
                        return Ok(Step::Wait {
                            deadline: program.deadline,
                        });
                    }
                    program.phase = Phase::CloseExclusive;
                }
                Phase::CloseExclusive => {
                    let ids = program
                        .exclusive
                        .iter()
                        .map(|resource| resource.id)
                        .collect();
                    match self.closers(&ids, &BTreeSet::new(), None, &mut program.waiting)? {
                        Some(ready) => effects
                            .extend(ready.into_iter().map(|resource| Effect::Close { resource })),
                        None => {
                            return Ok(Step::Wait {
                                deadline: program.deadline,
                            });
                        }
                    }
                    if effects.is_empty() {
                        program.phase = Phase::Open;
                    }
                }
                Phase::Open => {
                    let inner = self.inner.borrow();
                    if program.scopes.iter().any(|scope| {
                        inner.scopes[scope].dependencies.iter().any(|reference| {
                            inner
                                .references
                                .get(reference)
                                .filter(|reference| !reference.released)
                                .and_then(|reference| inner.objects.get(&reference.object))
                                .is_none_or(|object| object.phase == crate::Status::Gone)
                        })
                    }) {
                        return Err(Error::new(
                            Code::Unavailable,
                            Reason::Dependency,
                            "scope dependency failed",
                        ));
                    }
                    for id in &program.scopes {
                        let scope = &inner.scopes[id];
                        for parent in &scope.parents {
                            if !inner.scopes[parent].active && !program.scope_set.contains(parent) {
                                program.waiting.insert(WaitKey::Scope(*parent));
                            }
                        }
                        for reference_id in &scope.dependencies {
                            if let Some(reference) = inner.references.get(reference_id)
                                && inner
                                    .objects
                                    .get(&reference.object)
                                    .is_some_and(|object| object.phase == crate::Status::Loading)
                            {
                                program.waiting.insert(WaitKey::Object(reference.object));
                                program.waiting.insert(WaitKey::Reference(*reference_id));
                            }
                        }
                    }
                    if !program.waiting.is_empty() {
                        return Ok(Step::Wait {
                            deadline: program.deadline,
                        });
                    }
                    let mut missing = false;
                    let mut unique = BTreeSet::new();
                    for scope in &program.scopes {
                        for id in &inner.scopes[scope].resources {
                            let resource = inner
                                .resources
                                .get(id)
                                .ok_or_else(|| gone(Reason::Resource))?;
                            if resource.value.is_none() {
                                missing = true;
                                if resource.dependencies.iter().all(|id| {
                                    inner
                                        .resources
                                        .get(id)
                                        .is_some_and(|resource| resource.value.is_some())
                                }) && unique.insert(*id)
                                {
                                    effects.push(Effect::Open { resource: *id });
                                }
                            }
                        }
                    }
                    if missing && effects.is_empty() {
                        return Err(conflict(Reason::ResourceDependencies));
                    }
                    if !missing {
                        program.phase = Phase::Prepare;
                    }
                }
                Phase::Prepare => {
                    let inner = self.inner.borrow();
                    effects.extend(
                        program
                            .scopes
                            .iter()
                            .filter(|scope| program.ticket.is_some() || !inner.scopes[scope].active)
                            .map(|scope| Effect::Prepare {
                                scope: *scope,
                                previous: program.previous_scopes.get(scope).copied().flatten(),
                            }),
                    );
                    program.phase = Phase::State;
                }
                Phase::State => {
                    let inner = self.inner.borrow();
                    if program.ticket.is_some() {
                        for id in &program.scopes {
                            let scope = &inner.scopes[id];
                            if let (Owner::Object(object), Some(version)) =
                                (scope.owner, scope.state_version)
                            {
                                let snapshot = inner.objects[&object].state.unwrap_or(Snapshot {
                                    value: 0,
                                    revision: 0,
                                    version: 0,
                                });
                                program.expected.insert(*id, snapshot);
                                if snapshot.version == 0 {
                                    effects.push(Effect::Initialize { scope: *id });
                                } else if version > snapshot.version {
                                    effects.push(Effect::Migrate {
                                        scope: *id,
                                        value: Some(snapshot.value),
                                        version: snapshot.version,
                                    });
                                } else {
                                    effects.push(Effect::Validate {
                                        scope: *id,
                                        value: snapshot.value,
                                    });
                                }
                            }
                        }
                        program.phase = Phase::Prepared;
                    } else {
                        program.phase = Phase::Start;
                    }
                }
                Phase::Prepared => {
                    program.deadline = None;
                    return Ok(Step::Prepared);
                }
                Phase::Adopt => {
                    effects.push(Effect::Adopt {
                        replacements: program.replacements.clone(),
                    });
                    program.phase = Phase::Start;
                }
                Phase::Start | Phase::Resume => {
                    if self.inner.borrow().closing {
                        program.phase = if program.phase == Phase::Resume {
                            Phase::AbortDrop
                        } else {
                            Phase::Complete
                        };
                        continue;
                    }
                    if program.ticket.is_none() {
                        let objects: Vec<_> = {
                            let inner = self.inner.borrow();
                            program
                                .scopes
                                .iter()
                                .filter_map(|scope| match inner.scopes[scope].owner {
                                    Owner::Object(object)
                                        if inner.objects.get(&object).is_some_and(|record| {
                                            record.phase == crate::Status::Loading
                                                && !record.disposing
                                        }) =>
                                    {
                                        Some(object)
                                    }
                                    _ => None,
                                })
                                .collect()
                        };
                        for object in objects {
                            self.ready(object)?;
                        }
                    }
                    let mut inner = self.inner.borrow_mut();
                    let scopes: Vec<_> = if program.phase == Phase::Resume {
                        program
                            .replacements
                            .iter()
                            .filter_map(|pair| pair.previous)
                            .collect()
                    } else {
                        program.scopes.clone()
                    };
                    for scope in scopes {
                        let record = inner
                            .scopes
                            .get_mut(&scope)
                            .ok_or_else(|| gone(Reason::Scope))?;
                        if !record.active {
                            record.active = true;
                            effects.push(Effect::Start { scope });
                            inner.wake(WaitKey::Scope(scope));
                        }
                    }
                    program.phase = if program.phase == Phase::Resume {
                        Phase::AbortDrop
                    } else {
                        Phase::Complete
                    };
                }
                Phase::AbortStop => {
                    if program.ticket.is_none() {
                        let mut inner = self.inner.borrow_mut();
                        program.scopes = inner.failed_scope_dependents(&program.scopes);
                        program.scope_set = program.scopes.iter().copied().collect();
                        inner.begin_cleanup(&program.scopes);
                    }
                    effects.extend(program.scopes.iter().map(|scope| Effect::Stop {
                        scope: *scope,
                        keep_listeners: false,
                    }));
                    program.phase = Phase::AbortClose;
                }
                Phase::AbortClose if program.ticket.is_none() => {
                    match self.cleanup_batch(id, program)? {
                        Some(batch) => effects = batch,
                        None => return Ok(Step::Wait { deadline: None }),
                    }
                    if program.scopes.is_empty() {
                        program.phase = Phase::Drop;
                    }
                }
                Phase::AbortClose => {
                    let ids = self.scope_resources(&program.scopes);
                    match self.closers(&ids, &program.retained, None, &mut program.waiting)? {
                        Some(ready) => effects
                            .extend(ready.into_iter().map(|resource| Effect::Close { resource })),
                        None => return Ok(Step::Wait { deadline: None }),
                    }
                    if effects.is_empty() {
                        program.phase = if program.ticket.is_some() {
                            Phase::Restore
                        } else {
                            Phase::Drop
                        };
                    }
                }
                Phase::Restore => {
                    if self.inner.borrow().closing {
                        self.abort(program.ticket.expect("reload"))?;
                        program.phase = Phase::AbortDrop;
                        continue;
                    }
                    for old in &program.exclusive {
                        if self.inner.borrow().resources.contains_key(&old.id)
                            || program.restored.contains_key(&old.id)
                        {
                            continue;
                        }
                        let dependencies: Vec<_> = old
                            .dependencies
                            .iter()
                            .map(|id| program.restored.get(id).copied().unwrap_or(*id))
                            .collect();
                        if dependencies.iter().any(|id| {
                            self.inner
                                .borrow()
                                .resources
                                .get(id)
                                .is_none_or(|resource| resource.value.is_none())
                        }) {
                            continue;
                        }
                        let restored = self.resource(
                            old.owner,
                            old.name.clone(),
                            old.version,
                            old.exclusive,
                            dependencies,
                        )?;
                        self.inner
                            .borrow_mut()
                            .resources
                            .get_mut(&restored.id)
                            .expect("restored")
                            .code_version = old.code_version;
                        self.replace_resource(old.id, restored.id);
                        program.restored.insert(old.id, restored.id);
                        effects.push(Effect::Restore {
                            previous: old.id,
                            resource: restored.id,
                        });
                    }
                    if effects.is_empty() {
                        program.phase = Phase::Resume;
                        self.abort(program.ticket.expect("reload"))?;
                        for pair in &program.replacements {
                            if let Some(previous) = pair.previous
                                && let Some(scope) =
                                    self.inner.borrow_mut().scopes.get_mut(&previous)
                            {
                                scope.active = false;
                                effects.push(Effect::Prepare {
                                    scope: previous,
                                    previous: Some(previous),
                                });
                            }
                        }
                    }
                }
                Phase::Drop | Phase::AbortDrop => {
                    if program.ticket.is_some() && !program.committed {
                        for migration in &program.migrations {
                            self.inner
                                .borrow_mut()
                                .actions
                                .push_back(crate::Action::DropValue(migration.value));
                        }
                        program.migrations.clear();
                    }
                    for scope in &program.scopes {
                        if self.drop_scope(*scope)? {
                            effects.push(Effect::Drop { scope: *scope });
                        }
                    }
                    if let Some(object) = program.object.take() {
                        self.disposed(object)?;
                    }
                    program.phase = if program.ticket.is_some() {
                        Phase::Failed
                    } else {
                        Phase::Complete
                    };
                }
                Phase::Complete => {
                    return Ok(if let Some(error) = &program.error {
                        Step::Failed(error.clone())
                    } else {
                        Step::Complete
                    });
                }
                Phase::Failed => {
                    return Ok(Step::Failed(
                        program.error.clone().expect("aborted program"),
                    ));
                }
            }
            if !effects.is_empty() {
                for effect in &effects {
                    if let Effect::Stop {
                        scope,
                        keep_listeners: _keep,
                    } = effect
                    {
                        if let Some(scope) = self.inner.borrow_mut().scopes.get_mut(scope) {
                            scope.active = false;
                        }
                        #[cfg(feature = "subscriptions")]
                        self.stop_scope_listeners(*scope, *_keep)?;
                    }
                }
                program.pending = effects.clone();
                return Ok(Step::Execute(effects));
            }
        }
    }

    fn drop_scope(&self, scope: Id) -> Result<bool> {
        let mut inner = self.inner.borrow_mut();
        let Some(record) = inner.scopes.remove(&scope) else {
            return Ok(false);
        };
        if !record.closing {
            inner.track_scope_version(&record, false);
        }
        inner.scope_code_versions.remove(&scope);
        if record.owner != Owner::Host {
            inner.non_host_scopes -= 1;
            if inner.non_host_scopes == 0 {
                inner.wake(WaitKey::HostScopes);
            }
        }
        inner.owner_scopes.remove(&record.owner, scope);
        inner.facet_scopes.remove(&record.facet, scope);
        for parent in &record.parents {
            inner.scope_children.remove(parent, scope);
            inner.wake(WaitKey::Scope(*parent));
        }
        for resource in &record.resources {
            inner.resource_scopes.remove(resource, scope);
            inner.resource_claims.remove(resource, scope);
            inner.wake(WaitKey::Resource(*resource));
        }
        inner.wake(WaitKey::Scope(scope));
        for reference in &record.dependencies {
            inner.reference_scopes.remove(reference, scope);
            if let Some(target) = inner.references.get(reference).map(|claim| claim.object) {
                inner.scope_dependencies.remove(&target, scope);
            }
        }
        drop(inner);
        for reference in record.dependencies {
            if let Owner::Object(object) = record.owner {
                self.detach_dependency(object, reference)?;
            }
            if self.inner.borrow().references.contains_key(&reference) {
                self.release(reference)?;
                self.forget_reference(reference)?;
            }
        }
        Ok(true)
    }

    /// Children settle before parent scopes; resource dependencies determine order within a scope.
    fn cleanup_batch(&self, program_id: Id, program: &mut Program) -> Result<Option<Vec<Effect>>> {
        let leaves: Vec<_> = {
            let inner = self.inner.borrow();
            program
                .scopes
                .iter()
                .filter(|id| {
                    let leased = inner
                        .scope_programs
                        .members(id)
                        .filter(|active| *active != program_id)
                        .collect::<Vec<_>>();
                    if !leased.is_empty() {
                        program
                            .waiting
                            .extend(leased.into_iter().map(WaitKey::Program));
                        return false;
                    }
                    if inner.closing
                        && inner.non_host_scopes != 0
                        && inner
                            .scopes
                            .get(id)
                            .is_some_and(|scope| scope.owner == Owner::Host)
                    {
                        program.waiting.insert(WaitKey::HostScopes);
                        return false;
                    }
                    if inner.scope_children.contains(id)
                        || inner
                            .scope_activations
                            .get(id)
                            .is_some_and(|active| *active != program_id)
                    {
                        program.waiting.insert(WaitKey::Scope(**id));
                        false
                    } else {
                        true
                    }
                })
                .copied()
                .collect()
        };
        if leaves.is_empty() {
            return Ok(program.scopes.is_empty().then(Vec::new));
        }
        let mut effects = Vec::new();
        let mut dropped = BTreeSet::new();
        for scope in leaves {
            let mut ids = self.scope_resources(&[scope]);
            ids.retain(|id| !self.inner.borrow().resource_claims.contains(id));
            let Some(ready) = self.closers(&ids, &program.retained, None, &mut program.waiting)?
            else {
                continue;
            };
            if ready.is_empty() {
                if self.drop_scope(scope)? {
                    effects.push(Effect::Drop { scope });
                }
                dropped.insert(scope);
            } else {
                effects.extend(ready.into_iter().map(|resource| Effect::Close { resource }));
            }
        }
        program.scopes.retain(|id| !dropped.contains(id));
        Ok((!effects.is_empty() || program.scopes.is_empty()).then_some(effects))
    }

    fn scope_resources(&self, scopes: &[Id]) -> BTreeSet<Id> {
        let inner = self.inner.borrow();
        scopes
            .iter()
            .filter_map(|id| inner.scopes.get(id))
            .flat_map(|scope| scope.resources.iter().copied())
            .collect()
    }

    // Scope leases prevent cleanup from racing a reload's pending adapter effects.
    fn wait_for_reloads(&self, id: Id, program: &mut Program) -> bool {
        let inner = self.inner.borrow();
        for scope in &program.scopes {
            program.waiting.extend(
                inner
                    .scope_programs
                    .members(scope)
                    .filter(|active| *active != id)
                    .map(WaitKey::Program),
            );
        }
        !program.waiting.is_empty()
    }
    fn closers(
        &self,
        ids: &BTreeSet<Id>,
        retained: &BTreeSet<Id>,
        before: Option<Version>,
        waiting: &mut BTreeSet<WaitKey>,
    ) -> Result<Option<Vec<Id>>> {
        let (remaining, ready) = {
            let inner = self.inner.borrow();
            let remaining: Vec<_> = ids
                .iter()
                .filter(|id| !retained.contains(id) && inner.resources.contains_key(id))
                .copied()
                .collect();
            let ready = remaining
                .iter()
                .filter(|id| !inner.dependents.contains(id) && !inner.resources[id].closing)
                .copied()
                .collect::<Vec<_>>();
            for id in &remaining {
                waiting.insert(WaitKey::Resource(*id));
            }
            (remaining, ready)
        };
        if remaining.is_empty() {
            return Ok(Some(Vec::new()));
        }
        if ready.is_empty() {
            return Ok(None);
        }
        let mut admitted = Vec::new();
        for resource in ready {
            let cutoff = before.or_else(|| {
                let inner = self.inner.borrow();
                let owner = inner.resources[&resource].owner;
                inner
                    .scope_versions
                    .versions(&owner)
                    .find(|(version, count)| {
                        *count > inner.resource_versions.count(&resource, *version)
                    })
                    .map(|(version, _)| version)
            });
            if self.close_resource_before(resource, cutoff)? {
                admitted.push(resource);
            } else {
                waiting.insert(WaitKey::Owner(
                    self.inner.borrow().resources[&resource].owner,
                ));
            }
        }
        Ok((!admitted.is_empty()).then_some(admitted))
    }
    fn replace_resource(&self, previous: Id, next: Id) {
        let mut inner = self.inner.borrow_mut();
        for scope in inner.resource_scopes.take(&previous) {
            let record = inner.scopes.get_mut(&scope).expect("indexed scope");
            for resource in &mut record.resources {
                if *resource == previous {
                    *resource = next;
                }
            }
            let claimed = !record.closing;
            inner.resource_scopes.insert(next, scope);
            inner.resource_claims.remove(&previous, scope);
            if claimed {
                inner.resource_claims.insert(next, scope);
                if let Some(version) = inner.scope_code_versions.get(&scope).copied() {
                    inner.resource_versions.remove(&previous, version);
                    inner.resource_versions.insert(next, version);
                }
            }
        }
        for resource in inner.dependents.take(&previous) {
            for dependency in &mut inner
                .resources
                .get_mut(&resource)
                .expect("indexed resource")
                .dependencies
            {
                if *dependency == previous {
                    *dependency = next;
                }
            }
            inner.dependents.insert(next, resource);
        }
        inner.wake(WaitKey::Resource(previous));
    }

    pub fn scope_exists(&self, id: Id) -> bool {
        self.inner.borrow().scopes.contains_key(&id)
    }
    pub fn scopes_for_owner(&self, owner: Owner) -> Vec<Id> {
        self.inner.borrow().owner_scopes.members(&owner).collect()
    }
    pub fn scopes_for_facet(&self, facet: Id) -> Vec<Id> {
        self.inner.borrow().facet_scopes.members(&facet).collect()
    }
    pub fn add_scope_resource(&self, scope: Id, resource: Id) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        let owner = inner
            .resources
            .get(&resource)
            .ok_or_else(|| gone(Reason::Resource))?
            .owner;
        let scope = inner
            .scopes
            .get_mut(&scope)
            .ok_or_else(|| gone(Reason::Scope))?;
        if scope.owner != owner {
            return Err(conflict(Reason::ScopeOwner));
        }
        if !scope.resources.contains(&resource) {
            scope.resources.push(resource);
            let id = scope.id;
            let claimed = !scope.closing;
            inner.resource_scopes.insert(resource, id);
            if claimed {
                inner.resource_claims.insert(resource, id);
                if let Some(version) = inner.scope_code_versions.get(&id).copied() {
                    inner.resource_versions.insert(resource, version);
                }
            }
        }
        Ok(())
    }

    pub fn run_program(&self, id: Id) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        inner
            .programs
            .get_mut(&id)
            .ok_or_else(|| gone(Reason::Program))?
            .running = true;
        inner.ready_programs.insert(id);
        Ok(())
    }

    /// Advances every runnable program. Adapters have no lifecycle loop or phase transitions.
    pub(crate) fn pump_programs(&self) {
        let ids = std::mem::take(&mut self.inner.borrow_mut().ready_programs);
        for id in ids {
            #[cfg(test)]
            {
                self.inner.borrow_mut().program_visits += 1;
            }
            let cancellation = {
                let mut inner = self.inner.borrow_mut();
                let now = inner.now;
                let program = inner.programs.get_mut(&id).expect("listed program");
                if program.deadline.is_some_and(|deadline| now >= deadline)
                    && program.error.is_none()
                {
                    program.error = Some(conflict(Reason::ReloadDeadline));
                    program.deadline = None;
                    program.running = true;
                }
                if program.error.is_some()
                    && !program.pending.is_empty()
                    && !program.cancelled_effects
                {
                    program.cancelled_effects = true;
                    Some(program.pending.clone())
                } else {
                    None
                }
            };
            if let Some(effects) = cancellation {
                self.inner
                    .borrow_mut()
                    .actions
                    .push_back(crate::Action::CancelEffects {
                        program: id,
                        effects,
                    });
            }
            let runnable = {
                let inner = self.inner.borrow();
                let program = &inner.programs[&id];
                program.running && program.pending.is_empty()
            };
            if !runnable {
                continue;
            }
            let step = match self.next_step(id) {
                Ok(step) => step,
                Err(error) => {
                    let mut inner = self.inner.borrow_mut();
                    let program = inner.programs.get_mut(&id).expect("listed program");
                    program.error.get_or_insert(error);
                    program.phase = if program.committed {
                        Phase::Failed
                    } else {
                        Phase::AbortStop
                    };
                    drop(inner);
                    match self.next_step(id) {
                        Ok(step) => step,
                        Err(error) => Step::Failed(error),
                    }
                }
            };
            let mut inner = self.inner.borrow_mut();
            match step {
                Step::Execute(effects) => {
                    inner.programs.get_mut(&id).expect("listed").running = false;
                    inner.actions.push_back(crate::Action::Execute {
                        program: id,
                        effects,
                    });
                }
                Step::Wait { .. } => {}
                Step::Prepared => {
                    inner.programs.get_mut(&id).expect("listed").running = false;
                    inner.actions.push_back(crate::Action::ProgramPrepared(id));
                }
                Step::Complete => {
                    inner.remove_program(id);
                    inner.actions.push_back(crate::Action::ProgramComplete {
                        program: id,
                        error: None,
                    });
                }
                Step::Failed(error) => {
                    if let Some(Owner::Object(object)) = inner.programs[&id].activation_owner {
                        if let Some(record) = inner.objects.get_mut(&object) {
                            record.phase = crate::Status::Gone;
                            record.pinned = false;
                        }
                        inner.maybe_dispose(object);
                    }
                    inner.remove_program(id);
                    inner.actions.push_back(crate::Action::ProgramComplete {
                        program: id,
                        error: Some(error),
                    });
                }
            }
        }
    }

    pub fn next_wake(&self) -> Option<u64> {
        let inner = self.inner.borrow();
        self.next_deadline()
            .map(|deadline| deadline.saturating_sub(inner.now))
    }

    /// Absolute monotonic deadline; adapters can retain an unchanged scheduled timer.
    pub fn next_deadline(&self) -> Option<u64> {
        let inner = self.inner.borrow();
        inner
            .program_deadlines
            .keys()
            .next()
            .copied()
            .into_iter()
            .chain(inner.idle.keys().next().copied())
            .min()
    }
}
impl Program {
    pub(crate) fn close_reload(&mut self) -> bool {
        if self.ticket.is_none() {
            return false;
        }
        if !self.committed {
            self.error.get_or_insert_with(|| {
                Error::new(Code::Cancelled, Reason::ReloadAborted, "reload was aborted")
            });
        }
        self.running = true;
        true
    }

    fn new(scopes: Vec<Id>, phase: Phase) -> Self {
        Self {
            ticket: None,
            replacements: Vec::new(),
            previous_scopes: BTreeMap::new(),
            scope_set: scopes.iter().copied().collect(),
            scopes,
            object: None,
            activation_scope: None,
            activation_owner: None,
            phase,
            pending: Vec::new(),
            migrations: Vec::new(),
            expected: BTreeMap::new(),
            objects: Vec::new(),
            facets: Vec::new(),
            exclusive: Vec::new(),
            retained: BTreeSet::new(),
            deadline: None,
            error: None,
            restored: BTreeMap::new(),
            committed: false,
            running: false,
            cancelled_effects: false,
            waiting: BTreeSet::new(),
        }
    }
}

impl crate::host::Inner {
    // Declarations waiting on a failed activation cannot outlive its unopened dependencies.
    fn failed_scope_dependents(&self, scopes: &[Id]) -> Vec<Id> {
        let mut pending = scopes.to_vec();
        let mut affected = BTreeSet::new();
        while let Some(id) = pending.pop() {
            if !affected.insert(id) {
                continue;
            }
            if let Some(scope) = self.scopes.get(&id) {
                pending.extend(
                    self.scope_children
                        .members(&id)
                        .filter(|child| !self.scopes[child].active),
                );
                for resource in &scope.resources {
                    for dependent in self.dependents.members(resource) {
                        pending.extend(
                            self.resource_scopes
                                .members(&dependent)
                                .filter(|scope| !self.scopes[scope].active),
                        );
                    }
                }
            }
        }
        affected.into_iter().collect()
    }
    pub(crate) fn begin_cleanup(&mut self, scopes: &[Id]) {
        for id in scopes {
            if let Some(program) = self.scope_activations.get(id).copied()
                && let Some(record) = self.programs.get_mut(&program)
            {
                record.error.get_or_insert_with(|| {
                    Error::new(Code::Cancelled, Reason::ScopeClosed, "scope is closing")
                });
                record.running = true;
                self.ready_programs.insert(program);
            }
            let Some(scope) = self.scopes.get(id).cloned() else {
                continue;
            };
            if !scope.closing {
                self.track_scope_version(&scope, false);
                self.scopes.get_mut(id).expect("checked scope").closing = true;
                for resource in scope.resources {
                    self.resource_claims.remove(&resource, *id);
                }
            }
        }
    }
    fn track_scope_version(&mut self, scope: &Scope, add: bool) {
        let Some(version) = self.scope_code_versions.get(&scope.id).copied() else {
            return;
        };
        if add {
            self.scope_versions.insert(scope.owner, version);
        } else {
            self.scope_versions.remove(&scope.owner, version);
        }
        for resource in &scope.resources {
            if add {
                self.resource_versions.insert(*resource, version);
            } else {
                self.resource_versions.remove(resource, version);
            }
        }
    }
}
