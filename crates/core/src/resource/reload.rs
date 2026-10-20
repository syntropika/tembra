use super::*;
use std::cell::Cell;

/// Managed ownership is explicit; public Resource handles only retain metadata.
#[derive(Clone)]
pub(crate) struct ResourceSet {
    host: Weak<HostInner>,
    records: Rc<Vec<Rc<ResourceRecord>>>,
    opened: Rc<Vec<Rc<ResourceRecord>>>,
}
impl ResourceSet {
    pub(crate) fn new(
        host: Weak<HostInner>,
        records: Vec<Rc<ResourceRecord>>,
        opened: Vec<Rc<ResourceRecord>>,
    ) -> Self {
        Self {
            host,
            records: Rc::new(records),
            opened: Rc::new(opened),
        }
    }
    pub(crate) fn records(&self) -> &[Rc<ResourceRecord>] {
        &self.records
    }
    pub(crate) fn opened(&self) -> &[Rc<ResourceRecord>] {
        &self.opened
    }
    pub(crate) fn retire(&self, retained: &[Self]) -> LocalBoxFuture<'static, Result<()>> {
        let records: Vec<_> = self
            .opened
            .iter()
            .rev()
            .filter(|record| {
                !retained
                    .iter()
                    .any(|set| set.records.iter().any(|other| Rc::ptr_eq(record, other)))
            })
            .cloned()
            .collect();
        async move { close_all(records).await }.boxed_local()
    }
}

struct Entry {
    record: Rc<ResourceRecord>,
    spec: Rc<ResourceSpec>,
    kept: bool,
}

/// Object-local candidate journal; no transaction coordinator is retained.
pub(crate) struct ResourceReload {
    installed: ResourceSet,
    entries: RefCell<Vec<Rc<Entry>>>,
    opened: RefCell<Vec<Rc<ResourceRecord>>>,
    attempted: RefCell<Vec<Rc<ResourceRecord>>>,
    signal: CancellationToken,
    started: Cell<bool>,
    adopted: Cell<bool>,
    aborted: Cell<bool>,
    ready: Rc<dyn Fn(Reference) -> LocalBoxFuture<'static, Result<()>>>,
}
impl ResourceReload {
    #[cfg(test)]
    pub(crate) fn new(installed: &ResourceSet) -> Rc<Self> {
        Self::new_with_ready(
            installed,
            Rc::new(|reference| async move { reference.ready().await }.boxed_local()),
        )
    }
    pub(crate) fn new_with_ready(
        installed: &ResourceSet,
        ready: Rc<dyn Fn(Reference) -> LocalBoxFuture<'static, Result<()>>>,
    ) -> Rc<Self> {
        Rc::new(Self {
            installed: installed.clone(),
            entries: RefCell::default(),
            opened: RefCell::default(),
            attempted: RefCell::default(),
            signal: CancellationToken::new(),
            started: Cell::new(false),
            adopted: Cell::new(false),
            aborted: Cell::new(false),
            ready,
        })
    }

    pub(crate) fn declare<T: 'static>(
        &self,
        name: String,
        declaration: ResourceDeclaration<T>,
    ) -> Result<Resource<T>> {
        if self.started.get() || self.adopted.get() || self.aborted.get() {
            return Err(RuntimeError::invalid(
                "declaration_scope",
                "resource recording has ended",
            ));
        }
        if name.is_empty()
            || self
                .entries
                .borrow()
                .iter()
                .any(|entry| entry.record.name == name)
        {
            return Err(RuntimeError::invalid(
                "duplicate",
                "resource names must be nonempty and unique",
            ));
        }
        if declaration.version == 0 || declaration.version > 9_007_199_254_740_991 {
            return Err(RuntimeError::invalid(
                "resource_version",
                "resource version must be a positive safe integer",
            ));
        }
        for dependency in &declaration.deps {
            if let ResourceDependency::Resource(resource) = dependency
                && !self
                    .entries
                    .borrow()
                    .iter()
                    .any(|entry| Rc::ptr_eq(&entry.record, &resource.0))
            {
                return Err(RuntimeError::invalid(
                    "resource_dependency",
                    "resource dependency must be declared earlier",
                ));
            }
        }
        let spec = ResourceSpec::erase(declaration);
        let kept = self
            .installed
            .records
            .iter()
            .find(|record| {
                let old = record.spec();
                record.name == name
                    && record.type_id == TypeId::of::<T>()
                    && old.version == spec.version
                    && old.exclusive == spec.exclusive
                    && same_dependencies(&old.deps, &spec.deps)
            })
            .cloned();
        let (record, kept) = match kept {
            Some(record) => (record, true),
            None => (
                Rc::new(ResourceRecord {
                    name,
                    type_id: TypeId::of::<T>(),
                    spec: RefCell::new(spec.clone()),
                    value: RefCell::new(None),
                    opening: RefCell::new(None),
                    unavailable: RefCell::new(None),
                }),
                false,
            ),
        };
        self.entries.borrow_mut().push(Rc::new(Entry {
            record: record.clone(),
            spec,
            kept,
        }));
        Ok(Resource {
            record,
            host: self.installed.host.clone(),
            marker: PhantomData,
        })
    }

    /// Runs trusted adoption checks only after factory recording has completed.
    pub(crate) fn preflight_adoption(&self) -> Result<()> {
        let entries = self.entries.borrow().clone();
        for entry in entries {
            if entry.kept
                && let Some(check) = &entry.spec.adoption_check
            {
                let value = entry.record.value.borrow().clone().ok_or_else(|| {
                    RuntimeError::unavailable("resource", "kept resource is not open")
                })?;
                check(value.as_ref())?;
            }
        }
        Ok(())
    }

    pub(crate) fn requires_gate(&self) -> bool {
        let entries = self.entries.borrow().clone();
        self.installed.records.iter().any(|old| {
            old.spec().exclusive && !entries.iter().any(|entry| Rc::ptr_eq(old, &entry.record))
        }) || entries
            .iter()
            .any(|entry| !entry.kept && entry.spec.exclusive)
    }

    fn gated(entry: &Entry, entries: &[Rc<Entry>]) -> bool {
        !entry.kept
            && (entry.spec.exclusive
                || entry.spec.deps.iter().any(|dep| {
                    if let ResourceDependency::Resource(resource) = dep {
                        entries
                            .iter()
                            .find(|other| Rc::ptr_eq(&other.record, &resource.0))
                            .is_some_and(|other| Self::gated(other, entries))
                    } else {
                        false
                    }
                }))
    }

    pub(crate) fn open_nonexclusive(
        self: &Rc<Self>,
        signal: CancellationToken,
    ) -> LocalBoxFuture<'static, Result<()>> {
        let this = self.clone();
        async move {
            this.started.set(true);
            this.preflight_adoption()?;
            let entries = this.entries.borrow().clone();
            let opens = entries
                .iter()
                .filter(|entry| !entry.kept && !Self::gated(entry, &entries))
                .map(|entry| this.open(entry.clone(), signal.clone()))
                .collect::<Vec<_>>();
            let results = futures::future::join_all(opens).await;
            this.clear_opening();
            results.into_iter().collect::<Result<Vec<_>>>()?;
            Ok(())
        }
        .boxed_local()
    }

    fn clear_opening(&self) {
        let entries = self.entries.borrow().clone();
        let removed = entries
            .iter()
            .filter(|entry| !entry.kept)
            .filter_map(|entry| entry.record.opening.borrow_mut().take())
            .collect::<Vec<_>>();
        drop(removed);
    }

    fn open(
        self: &Rc<Self>,
        entry: Rc<Entry>,
        external: CancellationToken,
    ) -> LocalBoxFuture<'static, Result<()>> {
        if entry.kept {
            return async { Ok(()) }.boxed_local();
        }
        if let Some(opening) = entry.record.opening.borrow().clone() {
            return opening.boxed_local();
        }
        let this = self.clone();
        let selected = entry.clone();
        let future = async move {
            external.check()?;
            this.signal.check()?;
            let entries = this.entries.borrow().clone();
            let deps = selected
                .spec
                .deps
                .iter()
                .map(|dep| match dep {
                    ResourceDependency::Reference(reference) => {
                        let reference = reference.clone();
                        let ready = (this.ready)(reference);
                        let cancelled = this.signal.cancelled();
                        let external = external.cancelled();
                        async move {
                            match futures::future::select(
                                ready,
                                futures::future::select(cancelled, external),
                            )
                            .await
                            {
                                futures::future::Either::Left((result, _)) => result,
                                futures::future::Either::Right((cancelled, _)) => {
                                    let cause = match cancelled {
                                        futures::future::Either::Left((cause, _))
                                        | futures::future::Either::Right((cause, _)) => cause,
                                    };
                                    Err(cause)
                                }
                            }
                        }
                        .boxed_local()
                    }
                    ResourceDependency::Resource(resource) => {
                        let dependency = entries
                            .iter()
                            .find(|other| Rc::ptr_eq(&other.record, &resource.0))
                            .cloned()
                            .expect("declared dependency exists");
                        this.open(dependency, external.clone())
                    }
                })
                .collect::<Vec<_>>();
            let results = futures::future::join_all(deps).await;
            results.into_iter().collect::<Result<Vec<_>>>()?;
            external.check()?;
            this.signal.check()?;
            let token = this.signal.child();
            let open = (selected.spec.open)(token.clone());
            // Cancellation notifies the opener but never substitutes for physical settlement.
            let value = match futures::future::select(open, external.cancelled()).await {
                futures::future::Either::Left((result, _)) => result?,
                futures::future::Either::Right((cause, pending)) => {
                    token.cancel(cause.clone());
                    let result = pending.await;
                    if let Ok(value) = result {
                        *selected.record.value.borrow_mut() = Some(value);
                        selected.record.close().await?;
                    }
                    return Err(cause);
                }
            };
            *selected.record.value.borrow_mut() = Some(value);
            this.opened.borrow_mut().push(selected.record.clone());
            if let Err(cause) = this.signal.check().and_then(|_| external.check()) {
                selected.record.close().await?;
                return Err(cause);
            }
            Ok(())
        }
        .boxed_local()
        .shared();
        *entry.record.opening.borrow_mut() = Some(future.clone());
        future.boxed_local()
    }

    pub(crate) fn replace_exclusive(
        self: &Rc<Self>,
        signal: CancellationToken,
    ) -> LocalBoxFuture<'static, Result<()>> {
        let this = self.clone();
        async move {
            signal.check()?;
            this.signal.check()?;
            let entries = this.entries.borrow().clone();
            let old = this
                .installed
                .opened
                .iter()
                .rev()
                .filter(|record| {
                    record.spec().exclusive
                        && !entries
                            .iter()
                            .any(|entry| Rc::ptr_eq(record, &entry.record))
                })
                .cloned()
                .collect::<Vec<_>>();
            for record in old {
                // Journal before the external close: an Err may follow a real close effect.
                this.attempted.borrow_mut().push(record.clone());
                record.close().await?;
            }
            let opens = entries
                .iter()
                .filter(|entry| Self::gated(entry, &entries))
                .map(|entry| this.open(entry.clone(), signal.clone()))
                .collect::<Vec<_>>();
            let results = futures::future::join_all(opens).await;
            this.clear_opening();
            results.into_iter().collect::<Result<Vec<_>>>()?;
            Ok(())
        }
        .boxed_local()
    }

    pub(crate) fn preflight(&self) -> Result<()> {
        if self.adopted.get() || self.aborted.get() {
            return Err(RuntimeError::invalid(
                "resource_candidate",
                "resource candidate is no longer pending",
            ));
        }
        self.signal.check()?;
        let entries = self.entries.borrow().clone();
        for entry in entries {
            if entry.record.value.borrow().is_none() {
                return Err(RuntimeError::unavailable(
                    "resource",
                    "candidate resource has not opened",
                ));
            }
            if let Some(cause) = entry.record.unavailable.borrow().clone() {
                return Err(cause);
            }
        }
        Ok(())
    }

    /// Caller preflights all domains before this callback-free adoption section.
    pub(crate) fn adopt(&self) -> (ResourceSet, RetiredResourceDeclarations) {
        self.adopted.set(true);
        let entries = self.entries.borrow().clone();
        let mut retired = Vec::new();
        for entry in &entries {
            if entry.kept {
                let old = entry.record.spec.replace(entry.spec.clone());
                retired.push(old);
            }
        }
        let set = self.declarations();
        (set, RetiredResourceDeclarations { specs: retired })
    }

    pub(crate) fn declarations(&self) -> ResourceSet {
        let records = self
            .entries
            .borrow()
            .iter()
            .map(|entry| entry.record.clone())
            .collect::<Vec<_>>();
        let mut opened = self
            .installed
            .opened
            .iter()
            .filter(|old| records.iter().any(|record| Rc::ptr_eq(old, record)))
            .cloned()
            .collect::<Vec<_>>();
        opened.extend(self.opened.borrow().iter().cloned());
        ResourceSet::new(self.installed.host.clone(), records, opened)
    }

    /// Observes actual installed records after abort physically joins candidate cleanup.
    /// A candidate closer error does not make an unrelated installed value unavailable.
    /// No error or cancellation signal is cleared by this observation.
    pub(crate) fn installed_available(&self) -> bool {
        self.installed
            .records
            .iter()
            .all(|record| record.value.borrow().is_some() && record.unavailable.borrow().is_none())
    }

    pub(crate) fn abort(
        self: &Rc<Self>,
        cause: RuntimeError,
    ) -> LocalBoxFuture<'static, Result<()>> {
        let this = self.clone();
        async move {
            if this.adopted.get() {
                return Err(RuntimeError::invalid(
                    "resource_candidate",
                    "cannot abort adopted resources",
                ));
            }
            this.aborted.set(true);
            this.signal.cancel(cause.clone());
            let entries = this.entries.borrow().clone();
            let opens = entries
                .iter()
                .filter_map(|entry| entry.record.opening.borrow().clone())
                .collect::<Vec<_>>();
            futures::future::join_all(opens).await;
            this.clear_opening();
            let opened = std::mem::take(&mut *this.opened.borrow_mut());
            let mut errors = Vec::new();
            if let Err(error) = close_all(opened.into_iter().rev().collect()).await {
                errors.push(error);
            }
            let attempted = std::mem::take(&mut *this.attempted.borrow_mut());
            for record in attempted.into_iter().rev() {
                let spec = record.spec();
                match (spec.open)(CancellationToken::new()).await {
                    Ok(value) => {
                        *record.value.borrow_mut() = Some(value);
                    }
                    Err(error) => {
                        let unavailable = RuntimeError::unavailable(
                            "resource_restore",
                            "installed resource restoration failed",
                        )
                        .with_details(serde_json::json!({"cause":error,"reload":cause}));
                        *record.unavailable.borrow_mut() = Some(unavailable.clone());
                        errors.push(unavailable);
                    }
                }
            }
            if errors.is_empty() {
                Ok(())
            } else {
                Err(RuntimeError::unavailable(
                    "resource_cleanup",
                    "reload resource cleanup or restoration failed",
                )
                .with_details(serde_json::json!({"reload":cause,"errors":errors})))
            }
        }
        .boxed_local()
    }
}

pub(crate) struct RetiredResourceDeclarations {
    specs: Vec<Rc<ResourceSpec>>,
}
impl RetiredResourceDeclarations {
    pub(crate) fn finish(self) {
        drop(self.specs);
    }
}

fn same_dependencies(left: &[ResourceDependency], right: &[ResourceDependency]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(a, b)| match (a, b) {
            (ResourceDependency::Resource(a), ResourceDependency::Resource(b)) => {
                Rc::ptr_eq(&a.0, &b.0)
            }
            (ResourceDependency::Reference(a), ResourceDependency::Reference(b)) => {
                a.same_dependency(b)
            }
            _ => false,
        })
}
async fn close_all(records: Vec<Rc<ResourceRecord>>) -> Result<()> {
    let mut errors = Vec::new();
    for record in records {
        if let Err(error) = record.close().await {
            errors.push(error);
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(
            RuntimeError::unavailable("resource_cleanup", "one or more resource closers failed")
                .with_details(serde_json::json!({"errors":errors})),
        )
    }
}

#[cfg(test)]
mod tests;
