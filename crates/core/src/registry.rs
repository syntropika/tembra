//! Host-owned contributions with immutable, execution-version-selected tables.

mod operation;
pub(crate) mod store;

pub use operation::RegisteredOperation;
pub(crate) use store::{PendingRegistryPublication, RegistryStore};

use crate::{Context, Declaration, ErrorCode, Host, Result, RuntimeError};
use serde_json::Value;
use std::{cell::Cell, collections::HashMap, rc::Rc};
use tembra_contracts::{ContractKind, Member};

/// Selection when different host objects contribute the same registry key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ConflictPolicy {
    /// Refuse the complete activating candidate, preserving the installed table.
    #[default]
    Error,
    /// Select the contribution with the latest stable service declaration ordinal.
    Last,
}

/// A host-independent registry definition. A Host binds one policy to each name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Registry {
    name: String,
    conflict: ConflictPolicy,
}

impl Registry {
    /// Creates a definition without attaching it to a Host or publishing contributions.
    ///
    /// # Errors
    /// Refuses an empty name. A conflicting same-name policy is refused when attached.
    ///
    /// ```
    /// use tembra_core::{ConflictPolicy, Registry};
    /// let registry = Registry::new("commands", ConflictPolicy::Error)?;
    /// assert_eq!(registry.name(), "commands");
    /// assert!(Registry::new("", ConflictPolicy::Last).is_err());
    /// assert_eq!(registry.conflict_policy(), ConflictPolicy::Error);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn new(name: impl Into<String>, conflict: ConflictPolicy) -> Result<Self> {
        let name = name.into();
        if name.is_empty() {
            return Err(RuntimeError::invalid(
                "registry_name",
                "registry name must be nonempty",
            ));
        }
        Ok(Self { name, conflict })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn conflict_policy(&self) -> ConflictPolicy {
        self.conflict
    }
}

/// Immutable contribution data and an optional checked owner method.
/// This value does not retain its object's factory or resources.
#[derive(Clone)]
pub struct RegistryEntry {
    data: Value,
    owner_id: u64,
    operation: Option<RegisteredOperation>,
}

impl RegistryEntry {
    pub fn data(&self) -> &Value {
        &self.data
    }

    pub fn owner_id(&self) -> u64 {
        self.owner_id
    }

    pub fn operation(&self) -> Option<&RegisteredOperation> {
        self.operation.as_ref()
    }
}

struct Table {
    entries: Vec<(String, RegistryEntry)>,
    lookup: Option<HashMap<String, usize>>,
}

impl Table {
    fn new(entries: Vec<(String, RegistryEntry)>) -> Self {
        // Keep small tables allocation-light. Wider tables retain ordered entries and add
        // immutable key copies solely for lookup; no object or operation ownership is added.
        let lookup = (entries.len() >= 32).then(|| {
            entries
                .iter()
                .enumerate()
                .map(|(position, (key, _))| (key.clone(), position))
                .collect()
        });
        Self { entries, lookup }
    }
}

/// An ordered complete table that remains unchanged after later publication or close.
/// Data remains readable after disposal; managed operations still check live admission.
#[derive(Clone)]
pub struct RegistrySnapshot {
    table: Rc<Table>,
    version: u64,
}

impl RegistrySnapshot {
    pub fn code_version(&self) -> u64 {
        self.version
    }

    pub fn get(&self, key: &str) -> Option<&RegistryEntry> {
        if let Some(lookup) = &self.table.lookup {
            return lookup
                .get(key)
                .map(|&position| &self.table.entries[position].1);
        }
        self.table
            .entries
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, entry)| entry)
    }

    pub fn entries(&self) -> impl ExactSizeIterator<Item = (&str, &RegistryEntry)> {
        self.table
            .entries
            .iter()
            .map(|(key, entry)| (key.as_str(), entry))
    }

    pub fn len(&self) -> usize {
        self.table.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.table.entries.is_empty()
    }
}

/// A weak Host view. Retaining it does not keep services or the Host alive.
#[derive(Clone)]
pub struct RegistryView {
    store: std::rc::Weak<RegistryStore>,
    name: String,
}

impl RegistryView {
    /// Selects the context's exact admitted version, or the current version without one.
    ///
    /// # Errors
    /// Refuses a closed/dropped Host, a foreign/expired/cancelled context, and missing or
    /// retired versions. It never silently substitutes the current execution version.
    pub fn table(&self, context: Option<&Context>) -> Result<RegistrySnapshot> {
        self.store()?.table(&self.name, context)
    }

    /// Registers an observer and synchronously delivers the complete current table.
    /// Later notifications share the Host state/registry publication FIFO. Errors from
    /// observers are reported separately; they do not undo publication or unregister it.
    ///
    /// # Errors
    /// Refuses closed Hosts and object-factory subscription. Refusal installs no listener.
    pub fn subscribe(
        &self,
        callback: impl Fn(RegistrySnapshot) -> Result<()> + 'static,
    ) -> Result<RegistrySubscription> {
        self.store()?.subscribe(&self.name, Rc::new(callback))
    }

    fn store(&self) -> Result<Rc<RegistryStore>> {
        self.store
            .upgrade()
            .ok_or_else(|| RuntimeError::unavailable("closed", "host no longer exists"))
    }
}

/// An independently owned observation. Unsubscribe and Drop are idempotent; a removed
/// listener is skipped even if its ID was captured by an already queued publication.
pub struct RegistrySubscription {
    store: std::rc::Weak<RegistryStore>,
    id: Cell<Option<u64>>,
}

impl RegistrySubscription {
    pub fn unsubscribe(&self) {
        if let Some(id) = self.id.take()
            && let Some(store) = self.store.upgrade()
        {
            store.unsubscribe(id);
        }
    }
}

impl Drop for RegistrySubscription {
    fn drop(&mut self) {
        self.unsubscribe();
    }
}

#[derive(Clone)]
pub(crate) struct RegistrationDeclaration {
    registry: Registry,
    key: String,
    data: Value,
    method: Option<String>,
}

impl Host {
    /// Binds a registry definition and returns a weak current/exact-version view.
    ///
    /// # Errors
    /// Refuses a closed Host or a different policy already bound to this name.
    ///
    /// ```
    /// use serde_json::json;
    /// use tembra_core::{ConflictPolicy, Host, HostOptions, ManualDriver, Registry};
    /// let driver = ManualDriver::new();
    /// let host = Host::new(HostOptions::new("example", json!("host")), driver.clone());
    /// let registry = Registry::new("commands", ConflictPolicy::Error)?;
    /// let view = host.registry(&registry)?;
    /// let before = view.table(None)?;
    /// let conflicting = Registry::new("commands", ConflictPolicy::Last)?;
    /// assert!(host.registry(&conflicting).is_err());
    /// assert!(before.is_empty());
    /// assert!(view.table(None)?.is_empty());
    /// driver.run_until(host.close())?;
    /// assert!(view.table(None).is_err());
    /// assert!(before.is_empty());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn registry(&self, registry: &Registry) -> Result<RegistryView> {
        self.inner.registries.view(registry)
    }
}

impl Declaration {
    /// Contributes immutable portable metadata when this host-lifetime object becomes live.
    ///
    /// # Errors
    /// Refuses non-host lifetime, duplicate (registry,key), inconsistent definitions and
    /// invalid portable data. Refusal leaves this object's declarations unchanged.
    pub fn register(
        &mut self,
        registry: &Registry,
        key: impl Into<String>,
        data: Value,
    ) -> Result<()> {
        self.register_entry(registry, key.into(), data, None)
    }

    /// Contributes metadata and a checked method of this object's local contract.
    /// A snapshot stores weak admission, never an unchecked handler closure.
    ///
    /// # Errors
    /// In addition to `register` errors, refuses absent and non-method contract members.
    pub fn register_method(
        &mut self,
        registry: &Registry,
        key: impl Into<String>,
        data: Value,
        member: impl Into<String>,
    ) -> Result<()> {
        self.register_entry(registry, key.into(), data, Some(member.into()))
    }

    fn register_entry(
        &mut self,
        registry: &Registry,
        key: String,
        data: Value,
        method: Option<String>,
    ) -> Result<()> {
        let candidate = self.reload_candidate()?;
        let contract = candidate
            .as_ref()
            .map(|candidate| candidate.service.contract())
            .unwrap_or(&self.object.contract);
        let lifetime = candidate
            .as_ref()
            .map(|candidate| candidate.service.options().lifetime)
            .unwrap_or(self.object.lifetime);
        let registrations = candidate
            .as_ref()
            .map(|candidate| &candidate.registrations)
            .unwrap_or(&self.object.registrations);
        if lifetime != crate::Lifetime::Host || contract.kind() != ContractKind::Local {
            return Err(RuntimeError::invalid(
                "not_host_lifetime",
                "only local host-lifetime objects may register",
            ));
        }
        if registrations
            .borrow()
            .iter()
            .any(|entry| entry.registry.name == registry.name && entry.key == key)
        {
            return Err(RuntimeError::invalid(
                "duplicate",
                "registry key is already declared by this object",
            ));
        }
        if let Some(method) = &method
            && !matches!(
                contract.members("").and_then(|members| members.get(method)),
                Some(Member::Method { .. })
            )
        {
            return Err(RuntimeError::invalid(
                "registry_method",
                "registration must name an exposed contract method",
            ));
        }
        let data = tembra_contracts::canonical_json(&data)
            .and_then(|json| tembra_contracts::parse_json(&json))
            .map_err(crate::host::contract_error)?;
        self.host.registries.bind(registry)?;
        registrations.borrow_mut().push(RegistrationDeclaration {
            registry: registry.clone(),
            key,
            data,
            method,
        });
        Ok(())
    }
}

fn superseded() -> RuntimeError {
    RuntimeError::new(
        ErrorCode::Superseded,
        "code_version",
        "registry execution version is unavailable or retired",
    )
}
