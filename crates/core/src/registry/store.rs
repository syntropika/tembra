use super::{
    ConflictPolicy, RegisteredOperation, RegistrationDeclaration, Registry, RegistryEntry,
    RegistrySnapshot, RegistrySubscription, RegistryView, Table, operation::Epoch, superseded,
};
use crate::{Context, ErrorCode, Result, RuntimeError, host::HostInner, object::ObjectRecord};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::{Rc, Weak},
};

type Callback = Rc<dyn Fn(RegistrySnapshot) -> Result<()>>;
type Ordinals = BTreeMap<String, BTreeMap<String, u64>>;

struct Listener {
    name: String,
    callback: Callback,
}

#[derive(Clone)]
struct Owner {
    object: Weak<ObjectRecord>,
    ordinal: u64,
    contract: Rc<tembra_contracts::Contract>,
    declarations: Vec<RegistrationDeclaration>,
}

struct EpochTables {
    epoch: Rc<Epoch>,
    owners: BTreeMap<u64, Owner>,
    tables: BTreeMap<String, Rc<Table>>,
}

struct Installed {
    definitions: BTreeMap<String, ConflictPolicy>,
    epochs: BTreeMap<u64, EpochTables>,
    current: u64,
    order: Ordinals,
    generation: u64,
}

pub(crate) struct RegistryStore {
    host: Weak<HostInner>,
    installed: RefCell<Installed>,
    listeners: RefCell<BTreeMap<u64, Listener>>,
    next_listener: Cell<u64>,
}

struct Candidate {
    version: u64,
    owners: BTreeMap<u64, Owner>,
    tables: BTreeMap<String, Rc<Table>>,
}

/// A fully checked candidate. Adoption has no user callbacks; replaced table owners stay
/// here until finish, outside the Host's atomic table/state/phase adoption region.
pub(crate) struct PreparedTables {
    store: Weak<RegistryStore>,
    generation: u64,
    checked_version: Option<u64>,
    candidates: Vec<Candidate>,
    // None is a checked no-op: it owns no replacement tables or ordinal ledger.
    order: Option<Ordinals>,
    retired: Vec<Rc<Table>>,
    adopted: bool,
}

impl PreparedTables {
    pub(crate) fn finish(self) {
        drop(self);
    }
}

pub(crate) struct PendingRegistryPublication {
    store: Weak<RegistryStore>,
    snapshot: RegistrySnapshot,
    listeners: Vec<u64>,
}

impl PendingRegistryPublication {
    pub(crate) fn deliver(self) {
        let Some(store) = self.store.upgrade() else {
            return;
        };
        for id in self.listeners {
            let callback = store
                .listeners
                .borrow()
                .get(&id)
                .map(|listener| listener.callback.clone());
            if let Some(callback) = callback
                && let Err(error) = callback(self.snapshot.clone())
            {
                store.report(error);
            }
        }
    }
}

pub(crate) struct PreparedRegistryEpoch {
    generation: u64,
    epoch: Option<EpochTables>,
    order: Ordinals,
}
impl RegistryStore {
    pub(crate) fn new(host: Weak<HostInner>, initial: Rc<Epoch>) -> Rc<Self> {
        Rc::new(Self {
            host,
            installed: RefCell::new(Installed {
                definitions: BTreeMap::new(),
                epochs: BTreeMap::from([(
                    1,
                    EpochTables {
                        epoch: initial,
                        owners: BTreeMap::new(),
                        tables: BTreeMap::new(),
                    },
                )]),
                current: 1,
                order: BTreeMap::new(),
                generation: 0,
            }),
            listeners: RefCell::new(BTreeMap::new()),
            next_listener: Cell::new(0),
        })
    }

    pub(super) fn check_host(&self) -> Result<Rc<HostInner>> {
        let host = self
            .host
            .upgrade()
            .ok_or_else(|| RuntimeError::unavailable("closed", "host no longer exists"))?;
        if host.is_closed() {
            return Err(RuntimeError::unavailable("closed", "host is closed"));
        }
        Ok(host)
    }

    pub(super) fn bind(&self, registry: &Registry) -> Result<()> {
        self.check_host()?;
        let mut installed = self.installed.borrow_mut();
        match installed.definitions.get(&registry.name) {
            Some(policy) if *policy != registry.conflict => {
                let mut error = RuntimeError::invalid(
                    "registry_definition",
                    "registry name already has a different conflict policy",
                );
                error.set_details(serde_json::json!({"registry": registry.name}));
                Err(error)
            }
            Some(_) => Ok(()),
            None => {
                installed
                    .definitions
                    .insert(registry.name.clone(), registry.conflict);
                Ok(())
            }
        }
    }

    pub(super) fn view(self: &Rc<Self>, registry: &Registry) -> Result<RegistryView> {
        self.bind(registry)?;
        Ok(RegistryView {
            store: Rc::downgrade(self),
            name: registry.name.clone(),
        })
    }

    pub(super) fn table(&self, name: &str, context: Option<&Context>) -> Result<RegistrySnapshot> {
        let host = self.check_host()?;

        let mut observation = crate::observability::start(
            &host.observations,
            crate::observability::Operation::RegistryRead,
            name,
            None,
            None,
            None,
        );
        let result = (|| {
            if let Some(context) = context {
                context.check(host.id)?;
                if let Some(observation) = &mut observation {
                    observation.context(context);
                }
            }
            let installed = self.installed.borrow();
            let version = context.map_or(installed.current, Context::code_version);
            let epoch = installed
                .epochs
                .get(&version)
                .filter(|epoch| epoch.epoch.admitted.get())
                .ok_or_else(superseded)?;
            Ok(RegistrySnapshot {
                table: epoch
                    .tables
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| Rc::new(Table::new(Vec::new()))),
                version,
            })
        })();
        if let Some(observation) = &mut observation {
            observation.result(&result);
        }
        result
    }

    pub(super) fn subscribe(
        self: &Rc<Self>,
        name: &str,
        callback: Callback,
    ) -> Result<RegistrySubscription> {
        let host = self.check_host()?;
        if host.in_factory.get() != 0 {
            return Err(RuntimeError::new(
                ErrorCode::NotReady,
                "factory",
                "registry subscriptions cannot start in an object factory",
            ));
        }
        let snapshot = self.table(name, None)?;
        let id = self.next_listener.get().checked_add(1).ok_or_else(|| {
            RuntimeError::unavailable("registry_listeners", "registry listener identity exhausted")
        })?;
        self.next_listener.set(id);
        self.listeners.borrow_mut().insert(
            id,
            Listener {
                name: name.into(),
                callback: callback.clone(),
            },
        );
        let subscription = RegistrySubscription {
            store: Rc::downgrade(self),
            id: Cell::new(Some(id)),
        };
        if let Err(error) = callback(snapshot) {
            self.report(error);
        }
        Ok(subscription)
    }

    pub(super) fn unsubscribe(&self, id: u64) {
        let removed = self.listeners.borrow_mut().remove(&id);
        drop(removed);
    }

    fn report(&self, error: RuntimeError) {
        if let Some(host) = self.host.upgrade() {
            host.errors.borrow_mut().push(error);
        }
    }

    pub(crate) fn prepare_activation(
        self: &Rc<Self>,
        owner: &Rc<ObjectRecord>,
        declarations: &[RegistrationDeclaration],
        version: u64,
    ) -> Result<PreparedTables> {
        self.check_host()?;
        let installed = self.installed.borrow();
        let epoch = installed
            .epochs
            .get(&version)
            .filter(|epoch| epoch.epoch.admitted.get())
            .ok_or_else(superseded)?;
        if declarations.is_empty() && !epoch.owners.contains_key(&owner.id) {
            return Ok(self.no_change(&installed, Some(version)));
        }
        let mut owners = epoch.owners.clone();
        owners.remove(&owner.id);
        if !declarations.is_empty() {
            owners.insert(
                owner.id,
                Owner {
                    object: Rc::downgrade(owner),
                    ordinal: owner.service_ordinal,
                    contract: owner.contract.clone(),
                    declarations: declarations.to_vec(),
                },
            );
        }
        let mut order = installed.order.clone();
        let tables = self.build_tables(&owners, &epoch.epoch, &epoch.tables, &mut order)?;
        Ok(PreparedTables {
            store: Rc::downgrade(self),
            generation: installed.generation,
            checked_version: Some(version),
            candidates: vec![Candidate {
                version,
                owners,
                tables,
            }],
            order: Some(order),
            retired: Vec::new(),
            adopted: false,
        })
    }

    pub(crate) fn remove_owner(self: &Rc<Self>, owner_id: u64) -> Result<PreparedTables> {
        self.check_host()?;
        let installed = self.installed.borrow();
        if !installed
            .epochs
            .values()
            .any(|epoch| epoch.owners.contains_key(&owner_id))
        {
            return Ok(self.no_change(&installed, None));
        }
        let mut order = installed.order.clone();
        let mut candidates = Vec::new();
        for (&version, epoch) in &installed.epochs {
            if !epoch.owners.contains_key(&owner_id) {
                continue;
            }
            let mut owners = epoch.owners.clone();
            owners.remove(&owner_id);
            let tables = self.build_tables(&owners, &epoch.epoch, &epoch.tables, &mut order)?;
            candidates.push(Candidate {
                version,
                owners,
                tables,
            });
        }
        Ok(PreparedTables {
            store: Rc::downgrade(self),
            generation: installed.generation,
            checked_version: None,
            candidates,
            order: Some(order),
            retired: Vec::new(),
            adopted: false,
        })
    }

    fn no_change(
        self: &Rc<Self>,
        installed: &Installed,
        checked_version: Option<u64>,
    ) -> PreparedTables {
        PreparedTables {
            store: Rc::downgrade(self),
            generation: installed.generation,
            checked_version,
            candidates: Vec::new(),
            order: None,
            retired: Vec::new(),
            adopted: false,
        }
    }

    pub(crate) fn preflight(&self, candidate: &PreparedTables) -> Result<()> {
        self.check_host()?;
        let installed = self.installed.borrow();
        if !std::ptr::eq(candidate.store.as_ptr(), self)
            || candidate.adopted
            || candidate.generation != installed.generation
        {
            return Err(RuntimeError::new(
                ErrorCode::Superseded,
                "registry_candidate",
                "registry candidate no longer matches installed tables",
            ));
        }
        if let Some(version) = candidate.checked_version
            && !installed
                .epochs
                .get(&version)
                .is_some_and(|epoch| epoch.epoch.admitted.get())
        {
            return Err(superseded());
        }
        for candidate in &candidate.candidates {
            if !installed
                .epochs
                .get(&candidate.version)
                .is_some_and(|epoch| epoch.epoch.admitted.get())
            {
                return Err(superseded());
            }
        }
        Ok(())
    }

    /// Caller preflights immediately before the Host's callback-free adoption region.
    pub(crate) fn adopt(
        self: &Rc<Self>,
        candidate: &mut PreparedTables,
    ) -> Vec<PendingRegistryPublication> {
        assert!(
            self.preflight(candidate).is_ok(),
            "registry candidate must be preflighted before adoption"
        );
        let Some(order) = candidate.order.take() else {
            candidate.adopted = true;
            return Vec::new();
        };
        let mut installed = self.installed.borrow_mut();
        let current = installed.current;
        let mut publications = Vec::new();
        for next in &mut candidate.candidates {
            let epoch = installed
                .epochs
                .get_mut(&next.version)
                .expect("preflight admitted epoch");
            if next.version == current {
                let listeners = self.listeners.borrow();
                for (name, table) in &next.tables {
                    if epoch
                        .tables
                        .get(name)
                        .is_some_and(|previous| Rc::ptr_eq(previous, table))
                    {
                        continue;
                    }
                    let ids = listeners
                        .iter()
                        .filter(|(_, listener)| listener.name == *name)
                        .map(|(&id, _)| id)
                        .collect::<Vec<_>>();
                    if !ids.is_empty() {
                        publications.push(PendingRegistryPublication {
                            store: Rc::downgrade(self),
                            snapshot: RegistrySnapshot {
                                table: table.clone(),
                                version: next.version,
                            },
                            listeners: ids,
                        });
                    }
                }
            }
            epoch.owners = std::mem::take(&mut next.owners);
            let retired = std::mem::replace(&mut epoch.tables, std::mem::take(&mut next.tables));
            candidate.retired.extend(retired.into_values());
        }
        installed.order = order;
        installed.generation += 1;
        candidate.adopted = true;
        publications
    }

    fn build_tables(
        self: &Rc<Self>,
        owners: &BTreeMap<u64, Owner>,
        epoch: &Rc<Epoch>,
        previous: &BTreeMap<String, Rc<Table>>,
        order: &mut Ordinals,
    ) -> Result<BTreeMap<String, Rc<Table>>> {
        let mut selected: BTreeMap<String, BTreeMap<String, (u64, u64, RegistryEntry)>> =
            BTreeMap::new();
        let mut owners = owners.iter().collect::<Vec<_>>();
        owners.sort_by_key(|(id, owner)| (owner.ordinal, **id));
        for (&owner_id, owner) in owners {
            let Some(_object) = owner.object.upgrade() else {
                continue;
            };
            for declaration in &owner.declarations {
                let entries = selected
                    .entry(declaration.registry.name.clone())
                    .or_default();
                if let Some((_, other_id, _)) = entries.get(&declaration.key)
                    && declaration.registry.conflict == ConflictPolicy::Error
                {
                    let mut error = RuntimeError::invalid(
                        "registry_conflict",
                        "different objects contributed the same registry key",
                    );
                    error.set_details(serde_json::json!({
                        "registry": declaration.registry.name,
                        "key": declaration.key,
                        "owners": [other_id, owner_id],
                    }));
                    return Err(error);
                }
                let ordinals = order.entry(declaration.registry.name.clone()).or_default();
                if !ordinals.contains_key(&declaration.key) {
                    let ordinal = u64::try_from(ordinals.len()).map_err(|_| {
                        RuntimeError::unavailable("registry_order", "registry order exhausted")
                    })?;
                    ordinals.insert(declaration.key.clone(), ordinal);
                }
                entries.insert(
                    declaration.key.clone(),
                    (
                        owner.ordinal,
                        owner_id,
                        RegistryEntry {
                            data: declaration.data.clone(),
                            owner_id,
                            operation: declaration.method.as_ref().map(|method| {
                                RegisteredOperation {
                                    host: self.host.clone(),
                                    owner: owner.object.clone(),
                                    epoch: Rc::downgrade(epoch),
                                    store: Rc::downgrade(self),
                                    contract: owner.contract.clone(),
                                    method: method.clone(),
                                }
                            }),
                        },
                    ),
                );
            }
        }
        let mut result = BTreeMap::new();
        // Preserve an empty table for removed registries so existing observers receive it.
        let names = previous
            .keys()
            .chain(selected.keys())
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        for name in &names {
            if result.contains_key(name) {
                continue;
            }
            let mut entries = selected
                .remove(name)
                .unwrap_or_default()
                .into_iter()
                .map(|(key, (_, _, entry))| (key, entry))
                .collect::<Vec<_>>();
            entries.sort_by_key(|(key, _)| {
                order
                    .get(name)
                    .and_then(|keys| keys.get(key))
                    .copied()
                    .unwrap_or(0)
            });
            let unchanged = previous
                .get(name)
                .filter(|table| same_entries(&table.entries, &entries));
            result.insert(
                name.clone(),
                unchanged
                    .cloned()
                    .unwrap_or_else(|| Rc::new(Table::new(entries))),
            );
        }
        Ok(result)
    }

    pub(crate) fn prepare_epoch(
        self: &Rc<Self>,
        epoch: Rc<Epoch>,
        replacements: &[(
            Rc<ObjectRecord>,
            Rc<tembra_contracts::Contract>,
            Vec<RegistrationDeclaration>,
        )],
        removed: &[u64],
    ) -> Result<PreparedRegistryEpoch> {
        self.check_host()?;
        let installed = self.installed.borrow();
        let previous = &installed.epochs[&installed.current];
        let mut owners = previous.owners.clone();
        for id in removed {
            owners.remove(id);
        }
        for (object, contract, declarations) in replacements {
            owners.remove(&object.id);
            if !declarations.is_empty() {
                owners.insert(
                    object.id,
                    Owner {
                        object: Rc::downgrade(object),
                        ordinal: object.service_ordinal,
                        contract: contract.clone(),
                        declarations: declarations.clone(),
                    },
                );
            }
        }
        let mut order = installed.order.clone();
        let tables = self.build_tables(&owners, &epoch, &previous.tables, &mut order)?;
        Ok(PreparedRegistryEpoch {
            generation: installed.generation,
            epoch: Some(EpochTables {
                epoch,
                owners,
                tables,
            }),
            order,
        })
    }
    pub(crate) fn preflight_epoch(&self, candidate: &PreparedRegistryEpoch) -> Result<()> {
        self.check_host()?;
        if candidate.generation != self.installed.borrow().generation || candidate.epoch.is_none() {
            return Err(superseded());
        }
        Ok(())
    }
    pub(crate) fn adopt_epoch(
        self: &Rc<Self>,
        candidate: &mut PreparedRegistryEpoch,
    ) -> Vec<PendingRegistryPublication> {
        let next = candidate.epoch.take().expect("epoch candidate preflighted");
        let version = next.epoch.version.get();
        let mut installed = self.installed.borrow_mut();
        let old = &installed.epochs[&installed.current];
        let mut publications = Vec::new();
        for (name, table) in &next.tables {
            let changed = old
                .tables
                .get(name)
                .is_none_or(|previous| !same_entries(&previous.entries, &table.entries));
            if changed {
                let listeners = self
                    .listeners
                    .borrow()
                    .iter()
                    .filter(|(_, listener)| listener.name == *name)
                    .map(|(id, _)| *id)
                    .collect::<Vec<_>>();
                publications.push(PendingRegistryPublication {
                    store: Rc::downgrade(self),
                    snapshot: RegistrySnapshot {
                        table: table.clone(),
                        version,
                    },
                    listeners,
                });
            }
        }
        installed.epochs.insert(version, next);
        installed.current = version;
        installed.order = std::mem::take(&mut candidate.order);
        installed.generation += 1;
        publications
    }
    pub(crate) fn retire_epoch(&self, version: u64) {
        let retired = self.installed.borrow_mut().epochs.remove(&version);
        drop(retired);
    }

    pub(crate) fn close(&self) {
        let epochs = {
            let mut installed = self.installed.borrow_mut();
            for epoch in installed.epochs.values() {
                epoch.epoch.admitted.set(false);
            }
            std::mem::take(&mut installed.epochs)
        };
        let listeners = std::mem::take(&mut *self.listeners.borrow_mut());
        drop(listeners);
        drop(epochs);
    }
}

fn same_entries(left: &[(String, RegistryEntry)], right: &[(String, RegistryEntry)]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|((a_key, a), (b_key, b))| {
            a_key == b_key
                && a.owner_id == b.owner_id
                && a.data == b.data
                && match (&a.operation, &b.operation) {
                    (Some(left), Some(right)) => left.same_target(right),
                    (None, None) => true,
                    _ => false,
                }
        })
}

#[cfg(test)]
mod tests;
