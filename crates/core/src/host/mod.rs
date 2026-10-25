use crate::Reason;
mod contexts;
mod facets;
mod lifecycle;
mod references;
mod resources;
mod tasks;
#[cfg(test)]
mod tests;

use crate::{
    Code, Error, Result,
    index::{Index, VersionCounts},
    model::*,
    program::WaitKey,
    state::Snapshot,
};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, VecDeque},
    rc::Rc,
};

#[derive(Clone)]
pub struct Host {
    pub(crate) inner: Rc<RefCell<Inner>>,
}

pub(crate) struct Inner {
    pub next: Id,
    pub version: Version,
    pub closing: bool,
    pub closed: bool,
    pub facets: BTreeMap<Id, InstalledFacet>,
    pub versions: BTreeMap<Version, BTreeMap<String, Id>>,
    pub services: BTreeMap<Version, BTreeMap<String, (Id, Service)>>,
    pub objects: BTreeMap<Id, Object>,
    pub addresses: BTreeMap<Address, Id>,
    pub references: BTreeMap<Id, Reference>,
    pub object_references: Index<Id>,
    pub roots: BTreeMap<Id, Root>,
    pub calls: BTreeMap<Id, Invocation>,
    pub resources: BTreeMap<Id, Resource>,
    pub tasks: BTreeMap<Id, Task>,
    pub actions: VecDeque<Action>,
    pub(crate) pending: Option<Pending>,
    pub now: u64,
    pub idle: BTreeMap<u64, BTreeSet<Id>>,
    pub facet_min: BTreeMap<Id, Version>,
    pub scopes: BTreeMap<Id, crate::Scope>,
    pub programs: BTreeMap<Id, crate::program::Program>,
    pub ready_programs: BTreeSet<Id>,
    pub program_deadlines: BTreeMap<u64, BTreeSet<Id>>,
    pub waiters: Index<WaitKey>,
    pub owner_scopes: Index<Owner>,
    pub scope_versions: VersionCounts<Owner>,
    pub scope_code_versions: BTreeMap<Id, Version>,
    pub scope_activations: BTreeMap<Id, Id>,
    pub scope_programs: Index<Id>,
    pub non_host_scopes: usize,
    pub resource_versions: VersionCounts<Id>,
    pub current_owners: BTreeSet<Id>,
    pub facet_scopes: Index<Id>,
    pub scope_children: Index<Id>,
    pub resource_scopes: Index<Id>,
    pub resource_claims: Index<Id>,
    pub owner_resources: Index<Owner>,
    pub resource_names: Index<(Owner, String)>,
    pub dependents: Index<Id>,
    pub owner_tasks: Index<Owner>,
    pub owner_calls: Index<Owner>,
    pub facet_objects: Index<Id>,
    pub facet_versions: Index<Id>,
    pub version_users: BTreeMap<Version, usize>,
    pub object_roots: Index<Id>,
    pub facet_roots: Index<Id>,
    pub facet_tasks: Index<Id>,
    pub object_dependents: Index<Id>,
    pub scope_dependencies: Index<Id>,
    pub reference_scopes: Index<Id>,
    #[cfg(test)]
    pub program_visits: usize,
    #[cfg(feature = "hooks")]
    pub contributions: Vec<crate::hooks::Contribution>,
    #[cfg(feature = "hooks")]
    pub plans: BTreeMap<(Version, String, String), Rc<[Id]>>,
    #[cfg(feature = "subscriptions")]
    pub subscriptions: BTreeMap<Id, crate::subscriptions::Connection>,
    #[cfg(feature = "subscriptions")]
    pub scope_listeners: Index<Id>,
    #[cfg(feature = "subscriptions")]
    pub owner_listeners: Index<Owner>,
    #[cfg(feature = "subscriptions")]
    pub ready_deliveries: BTreeSet<Id>,
    #[cfg(all(test, feature = "subscriptions"))]
    pub delivery_visits: usize,
}

pub(crate) struct Pending {
    pub(crate) reload: Reload,
    pub(crate) current: BTreeMap<String, Id>,
    pub(crate) objects: BTreeSet<Id>,
    facet_indices: BTreeMap<Id, usize>,
    services: BTreeMap<String, (Id, Service, Version)>,
    owners: BTreeSet<Id>,
    gated: BTreeSet<Id>,
    gated_facets: BTreeMap<Id, BTreeSet<Id>>,
}

impl Default for Host {
    fn default() -> Self {
        Self::new()
    }
}

impl Inner {
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
    pub fn wake(&mut self, key: WaitKey) {
        self.ready_programs.extend(self.waiters.take(&key));
    }
    pub fn settled_owner(&mut self, owner: Owner) {
        self.wake(WaitKey::Owner(owner));
        if owner != Owner::Host {
            self.wake(WaitKey::Owner(Owner::Host));
        }
    }
    pub fn id(&mut self) -> Result<Id> {
        reserve(&mut self.next)
    }
    pub fn current(&self) -> &BTreeMap<String, Id> {
        &self.versions[&self.version]
    }
    pub fn open(&self) -> Result<()> {
        if self.closing {
            Err(Error::new(Code::Closed, Reason::Host, "host is closing"))
        } else {
            Ok(())
        }
    }
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
    fn root(&mut self, version: Version, principal: String, cleanup: bool) -> Result<Id> {
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
                touched: BTreeSet::new(),
                facets: BTreeSet::new(),
                cleanup,
            },
        );
        Ok(id)
    }
    fn find_service(&self, name: &str, version: Version) -> Result<(Id, &Service)> {
        let services = self
            .services
            .get(&version)
            .ok_or_else(|| conflict(Reason::Superseded))?;
        services
            .get(name)
            .map(|(facet, service)| (*facet, service))
            .ok_or_else(|| Error::new(Code::Unavailable, Reason::UnknownService, name))
    }
    fn check_facet(&self, facet: Id, root: Id, version: Version) -> Result<Id> {
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
    fn facet_busy(&self, owner: Id) -> bool {
        self.facet_roots.contains(&owner) || self.facet_tasks.contains(&owner)
    }
    fn gate_object_busy(&self, object: Id) -> bool {
        self.object_roots.contains(&object) || self.objects[&object].tasks != 0
    }
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
    fn owner_busy(&self, owner: Owner, from: Version, before: Option<Version>) -> bool {
        let retained = |version| version >= from && before.is_none_or(|before| version < before);
        self.owner_calls
            .members(&owner)
            .any(|id| retained(self.calls[&id].code_version))
            || self
                .owner_tasks
                .members(&owner)
                .any(|id| retained(self.tasks[&id].version))
    }
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
    pub fn remove_root(&mut self, id: Id) {
        if let Some(root) = self.roots.remove(&id) {
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
    }
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

pub(crate) fn gone(kind: Reason) -> Error {
    Error::new(Code::Gone, kind, format!("{kind} is no longer available"))
}
pub(crate) fn conflict(reason: Reason) -> Error {
    Error::new(Code::Conflict, reason, reason.as_str().replace('_', " "))
}
fn exhausted() -> Error {
    Error::new(
        Code::Exhausted,
        Reason::Identities,
        "identity or revision space exhausted",
    )
}
fn reserve(next: &mut Id) -> Result<Id> {
    let id = *next;
    if id > MAX_ID {
        return Err(exhausted());
    }
    *next += 1;
    Ok(id)
}
fn validate_facet(facet: &Facet) -> Result<()> {
    if facet.name.is_empty() || facet.api_version == 0 {
        return Err(Error::invalid(
            Reason::Facet,
            "facet name and api version are required",
        ));
    }
    if !cfg!(feature = "hooks")
        && (!facet.hooks.is_empty() || facet.requires.iter().any(|r| !r.hooks.is_empty()))
    {
        return Err(Error::new(
            Code::Unsupported,
            Reason::Hooks,
            "hooks were excluded from this build",
        ));
    }
    let mut services = BTreeSet::new();
    for service in &facet.services {
        if service.name.is_empty()
            || !services.insert(&service.name)
            || service.methods.iter().any(|m| {
                m.is_empty()
                    || matches!(
                        m.as_str(),
                        "then" | "release" | "state" | "ready" | "status" | "opaque"
                    )
            })
        {
            return Err(Error::invalid(
                Reason::Service,
                "invalid or duplicate service/member name",
            ));
        }
        if service.lifetime == Lifetime::Host && service.linger_ms != 0 {
            return Err(Error::invalid(
                Reason::Linger,
                "host-lifetime objects cannot linger",
            ));
        }
    }
    Ok(())
}
