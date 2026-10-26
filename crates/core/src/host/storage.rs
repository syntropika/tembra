//! Authoritative records and their indexes.

use super::*;

#[derive(Default)]
pub(crate) struct Inner {
    pub next: Id,
    pub version: Version,
    pub closing: bool,
    pub closed: bool,
    pub orbs: BTreeMap<Id, InstalledOrb>,
    pub versions: BTreeMap<Version, BTreeMap<String, Id>>,
    pub services: BTreeMap<Version, BTreeMap<String, (Id, Service)>>,
    pub objects: BTreeMap<Id, Object>,
    pub addresses: BTreeMap<Address, Id>,
    pub references: BTreeMap<Id, Reference>,
    pub object_references: Index<Id>,
    pub roots: BTreeMap<Id, Root>,
    pub context_frames: BTreeMap<Id, crate::context_values::ContextFrame>,
    pub context_contracts: BTreeMap<(String, u32), crate::ContextKind>,
    pub context_peak: usize,
    pub call_contexts: BTreeMap<Id, Vec<Id>>,
    pub calls: BTreeMap<Id, Invocation>,
    pub resources: BTreeMap<Id, Resource>,
    pub tasks: BTreeMap<Id, Task>,
    pub actions: VecDeque<Action>,
    pub(crate) pending: Option<Pending>,
    pub now: u64,
    pub idle: BTreeMap<u64, BTreeSet<Id>>,
    pub orb_min: BTreeMap<Id, Version>,
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
    pub orb_scopes: Index<Id>,
    pub scope_children: Index<Id>,
    pub resource_scopes: Index<Id>,
    pub resource_claims: Index<Id>,
    pub owner_resources: Index<Owner>,
    pub resource_names: Index<(Owner, String)>,
    pub dependents: Index<Id>,
    pub owner_tasks: Index<Owner>,
    pub owner_calls: Index<Owner>,
    pub orb_objects: Index<Id>,
    pub orb_versions: Index<Id>,
    pub version_users: BTreeMap<Version, usize>,
    pub object_roots: Index<Id>,
    pub orb_roots: Index<Id>,
    pub orb_tasks: Index<Id>,
    pub object_dependents: Index<Id>,
    pub scope_dependencies: Index<Id>,
    pub reference_scopes: Index<Id>,
    #[cfg(test)]
    pub program_visits: usize,
    #[cfg(feature = "hooks")]
    pub contributions: Vec<crate::hooks::Contribution>,
    #[cfg(feature = "hooks")]
    pub observations: crate::hooks::observation::Observations,
    #[cfg(feature = "hooks")]
    pub contribution_ordinal: u32,
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
    pub(super) orb_indices: BTreeMap<Id, usize>,
    pub(super) services: BTreeMap<String, (Id, Service, Version)>,
    pub(super) owners: BTreeSet<Id>,
    pub(super) gated: BTreeSet<Id>,
    pub(super) gated_orbs: BTreeMap<Id, BTreeSet<Id>>,
}
