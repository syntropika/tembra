//! WIT resources inside one compiled Store.
//!
//! An exported guest resource never leaves the Store that created it. Its owned handle is filed
//! under the environment, meaning the kernel object, whose call returned it, and only a
//! host-issued instance id crosses to the bindings. An entry is removed before its destructor
//! runs, so it is destroyed at most once and a later reference to it fails before any lowering:
//! lowering a dead handle would fail the whole concurrent run.
//!
//! An imported resource is a dynamic host type whose representation is a slot in the
//! importer's own host handle table. The guest can lend it as `self` or drop it; it never names
//! another table.
use crate::Engine;
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use wasmtime::{
    AsContextMut, Store,
    component::{ResourceAny, ResourceDynamic, ResourceType, Val},
};

/// Exported guest resources of one Store, by issuing environment and host-issued id.
pub(crate) struct Instances {
    by_environment: HashMap<u64, HashMap<u32, ResourceAny>>,
    next: u32,
    /// The engine's count of live exported resources, for leak diagnostics.
    live: Arc<AtomicUsize>,
}

impl Instances {
    pub(crate) fn new(live: Arc<AtomicUsize>) -> Self {
        Self {
            by_environment: HashMap::new(),
            next: 0,
            live,
        }
    }

    /// File a freshly returned owned handle. Ids are never reused within a Store.
    pub(crate) fn insert(
        &mut self,
        environment: u64,
        resource: ResourceAny,
    ) -> wasmtime::Result<u32> {
        let id = self
            .next
            .checked_add(1)
            .ok_or_else(|| wasmtime::Error::msg("resource instance ids are exhausted"))?;
        self.next = id;
        self.by_environment
            .entry(environment)
            .or_default()
            .insert(id, resource);
        self.live.fetch_add(1, Ordering::AcqRel);
        Ok(id)
    }

    /// The live handle of `id`, only if this environment issued it with exactly this type.
    pub(crate) fn receiver(
        &self,
        environment: u64,
        id: u32,
        ty: &ResourceType,
    ) -> Option<ResourceAny> {
        self.by_environment
            .get(&environment)?
            .get(&id)
            .filter(|resource| resource.owned() && resource.ty() == *ty)
            .copied()
    }

    /// Remove one handle before it is destroyed.
    pub(crate) fn remove(&mut self, environment: u64, id: u32) -> Option<ResourceAny> {
        let table = self.by_environment.get_mut(&environment)?;
        let resource = table.remove(&id)?;
        if table.is_empty() {
            self.by_environment.remove(&environment);
        }
        self.live.fetch_sub(1, Ordering::AcqRel);
        Some(resource)
    }

    /// Remove every handle an environment issued, when that environment is released.
    pub(crate) fn take(&mut self, environment: u64) -> Vec<ResourceAny> {
        let resources: Vec<_> = self
            .by_environment
            .remove(&environment)
            .map(|table| table.into_values().collect())
            .unwrap_or_default();
        self.live.fetch_sub(resources.len(), Ordering::AcqRel);
        resources
    }
}

impl Drop for Instances {
    /// A Store torn down without draining still stops counting its handles.
    fn drop(&mut self) {
        let remaining = self.by_environment.values().map(HashMap::len).sum();
        self.live.fetch_sub(remaining, Ordering::AcqRel);
    }
}

/// Run the guest destructors of handles already removed from their table.
///
/// Each destructor gets its own run, fuel budget and wall bound, so one environment's many
/// instances cannot exhaust a shared budget. Refilling fuel per destructor is sound only because
/// this Store serves one command at a time; concurrent staging must not inherit it. Destructors
/// hold no call authority, so a destructor that imports from another Orb is refused. A trap or
/// timeout is a Store failure: the revision is quarantined with that cause, and the remaining
/// handles, already out of the table, are reclaimed with the Store without guest code.
pub(crate) async fn destroy<T: Send + 'static>(
    engine: &Engine,
    store: &mut Store<T>,
    resources: Vec<ResourceAny>,
) -> wasmtime::Result<()> {
    for resource in resources {
        store.set_fuel(engine.limits.fuel)?;
        let destroyed = store.run_concurrent(async move |accessor| {
            resource.resource_drop_concurrent(accessor).await
        });
        tokio::time::timeout(
            std::time::Duration::from_millis(engine.limits.timeout_ms),
            destroyed,
        )
        .await
        .map_err(|_| wasmtime::Error::msg("resource destructor timed out"))???;
    }
    Ok(())
}

/// The importer's table slot named by a guest's `self` borrow of one imported resource type.
///
/// Converting the borrow also releases the Store's temporary entry for it.
pub(crate) fn lent_slot(
    store: impl AsContextMut,
    value: &Val,
    payload: u32,
) -> wasmtime::Result<u32> {
    let Val::Resource(resource) = value else {
        return Err(wasmtime::Error::msg(
            "resource method receives no resource handle",
        ));
    };
    if resource.owned() {
        return Err(wasmtime::Error::msg(
            "only an implicit self borrow can name an imported resource",
        ));
    }
    let resource = ResourceDynamic::try_from_resource_any(*resource, store)?;
    if resource.ty() != payload {
        return Err(wasmtime::Error::msg("imported resource has another type"));
    }
    Ok(resource.rep())
}

/// Give the guest an owned handle to the importer's table slot.
pub(crate) fn owned_value(
    store: impl AsContextMut,
    slot: u32,
    payload: u32,
) -> wasmtime::Result<Val> {
    Ok(Val::Resource(
        ResourceDynamic::new_own(slot, payload).try_into_resource_any(store)?,
    ))
}
