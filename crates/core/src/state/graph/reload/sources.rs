use super::*;
use futures::{FutureExt, future::LocalBoxFuture};

pub(crate) struct CandidateSources {
    pub(super) plans: Vec<Rc<GraphReload>>,
    cache: RefCell<BTreeMap<(usize, NodeId), Rc<Value>>>,
    migrations: RefCell<BTreeMap<(usize, NodeId), Rc<Value>>>,
    projecting: Flag<bool>,
    frozen: Flag<bool>,
    external: RefCell<BTreeMap<usize, (Readable, Rc<Value>)>>,
    proxy_links: BTreeMap<usize, Readable>,
}
pub(crate) struct ReadyBarrier {
    sources: Rc<CandidateSources>,
    locks: Vec<(Rc<StateGraph>, Vec<NodeId>)>,
}
impl Drop for ReadyBarrier {
    fn drop(&mut self) {
        for (graph, ids) in &self.locks {
            let mut locks = graph.ready.borrow_mut();
            for id in ids {
                if let Some(count) = locks.get_mut(id) {
                    *count -= 1;
                    if *count == 0 {
                        locks.remove(id);
                    }
                }
            }
        }
        self.sources.frozen.set(false);
        let retained = self
            .sources
            .plans
            .iter()
            .filter(|plan| plan.adopted.get())
            .map(|plan| plan.declarations())
            .collect::<Vec<_>>();
        for (graph, _) in &self.locks {
            graph.flush_retirements(&retained);
        }
        for (graph, _) in &self.locks {
            graph.flush_refreshes();
        }
    }
}
pub(super) struct Projection<'a> {
    sources: &'a CandidateSources,
    previous: bool,
}
impl<'a> Projection<'a> {
    pub(super) fn new(sources: &'a CandidateSources) -> Self {
        let previous = sources.projecting.replace(true);
        if !previous {
            sources.cache.borrow_mut().clear();
        }
        Self { sources, previous }
    }
}
impl Drop for Projection<'_> {
    fn drop(&mut self) {
        self.sources.projecting.set(self.previous);
    }
}
struct CandidateRead {
    sources: Weak<CandidateSources>,
    plan: Weak<GraphReload>,
}
impl CandidateSources {
    pub(crate) fn new(plans: Vec<Rc<GraphReload>>) -> Rc<Self> {
        Rc::new(Self {
            plans,
            cache: RefCell::default(),
            migrations: RefCell::default(),
            projecting: Flag::new(false),
            frozen: Flag::new(false),
            external: RefCell::default(),
            proxy_links: BTreeMap::new(),
        })
    }
    /// Extends prospective validation to installed downstream graphs without replaying owners.
    /// Proxy links are selected from already checked implementation views by the host.
    pub(crate) fn with_dependents(
        self: &Rc<Self>,
        installed_graphs: Vec<GraphDeclarationSet>,
        proxy_links: Vec<(Readable, Readable)>,
    ) -> Result<Rc<Self>> {
        if self.frozen.get() || self.projecting.get() {
            return Err(RuntimeError::invalid(
                "reload_barrier",
                "projection is already frozen",
            ));
        }
        let mut links = self.proxy_links.clone();
        for (proxy, source) in proxy_links {
            if proxy.same_source(&source) {
                continue;
            }
            let key = proxy.port.as_ptr() as usize;
            if links.get(&key).is_some_and(|old| !old.same_source(&source)) {
                return Err(RuntimeError::invalid(
                    "state_source",
                    "conflicting proxy projections",
                ));
            }
            links.insert(key, source);
        }
        let mut sources = Self {
            plans: self.plans.clone(),
            cache: RefCell::default(),
            migrations: RefCell::new(self.migrations.borrow().clone()),
            projecting: Flag::new(false),
            frozen: Flag::new(false),
            external: RefCell::default(),
            proxy_links: links,
        };
        // Validate every alias before discovering dependencies, including aliases not imported yet.
        for source in sources.proxy_links.values() {
            sources.project_source(source)?;
        }
        loop {
            let mut added = false;
            for installed in &installed_graphs {
                let graph = installed.graph.upgrade().ok_or_else(gone)?;
                if sources
                    .plans
                    .iter()
                    .any(|plan| Rc::ptr_eq(&plan.graph, &graph))
                {
                    continue;
                }
                let nodes = installed
                    .nodes
                    .iter()
                    .map(|id| graph.node(*id))
                    .collect::<Result<Vec<_>>>()?;
                let mut affected = false;
                for node in nodes.into_iter().filter(|node| node.installed.get()) {
                    if let Kind::Import { source, .. } = &node.kind {
                        let source = sources.project_source(source)?;
                        affected |= source.origin.as_ref().is_some_and(|(upstream, _)| {
                            sources
                                .plans
                                .iter()
                                .any(|plan| Weak::ptr_eq(upstream, &Rc::downgrade(&plan.graph)))
                        });
                    }
                }
                if affected {
                    let mut plan = graph.begin_reload(Rc::new(|| {
                        Err(RuntimeError::invalid(
                            "declaration_scope",
                            "projection does not record declarations",
                        ))
                    }))?;
                    let projection = Rc::get_mut(&mut plan).expect("new projection has no aliases");
                    projection.projection_only = true;
                    projection.baseline = installed.clone();
                    sources.plans.push(plan);
                    added = true;
                }
            }
            if !added {
                break;
            }
        }
        Ok(Rc::new(sources))
    }
    fn project_source(&self, source: &Readable) -> Result<Readable> {
        let mut current = source.clone();
        let mut seen = Vec::new();
        while let Some(next) = self.proxy_links.get(&(current.port.as_ptr() as usize)) {
            let key = current.port.as_ptr() as usize;
            if seen.contains(&key) {
                return Err(RuntimeError::invalid(
                    "state_cycle",
                    "prospective proxy cycle",
                ));
            }
            seen.push(key);
            current = next.clone();
        }
        Ok(current)
    }
    pub(crate) fn ready_barrier(self: &Rc<Self>) -> Result<ReadyBarrier> {
        if self.frozen.get() {
            return Err(RuntimeError::invalid(
                "reload_barrier",
                "ready barrier is already held",
            ));
        }
        fn collect(
            sources: &CandidateSources,
            graph: &Rc<StateGraph>,
            id: NodeId,
            seen: &mut BTreeMap<(usize, NodeId), Rc<StateGraph>>,
            external: &mut BTreeMap<usize, (Readable, Rc<Value>)>,
        ) -> Result<()> {
            let key = (Rc::as_ptr(graph) as usize, id);
            if seen.insert(key, graph.clone()).is_some() {
                return Ok(());
            }
            let node = graph.node(id)?;
            match &node.kind {
                Kind::Derived {
                    sources: inputs, ..
                } => {
                    for source in inputs {
                        collect(sources, graph, *source, seen, external)?;
                    }
                }
                Kind::Import { source, .. } => {
                    let source = sources.project_source(source)?;
                    if let Some((other, id)) = &source.origin {
                        collect(
                            sources,
                            &other.upgrade().ok_or_else(gone)?,
                            *id,
                            seen,
                            external,
                        )?;
                    } else {
                        external.insert(
                            source.port.as_ptr() as usize,
                            (source.clone(), source.snapshot()?.shared_value()),
                        );
                    }
                }
                Kind::Cell => {}
            }
            Ok(())
        }
        let mut seen = BTreeMap::new();
        let mut external = BTreeMap::new();
        for plan in &self.plans {
            let ids = if plan.projection_only {
                plan.baseline.nodes.as_ref().clone()
            } else {
                plan.graph
                    .nodes
                    .borrow()
                    .iter()
                    .enumerate()
                    .filter_map(|(id, node)| {
                        node.as_ref()
                            .filter(|node| {
                                node.installed.get() || plan.owned.borrow().contains(&id)
                            })
                            .map(|_| id)
                    })
                    .collect::<Vec<_>>()
            };
            for id in ids {
                collect(self, &plan.graph, id, &mut seen, &mut external)?;
            }
        }
        let mut grouped: BTreeMap<usize, (Rc<StateGraph>, Vec<NodeId>)> = BTreeMap::new();
        for ((key, id), graph) in seen {
            grouped
                .entry(key)
                .or_insert_with(|| (graph, Vec::new()))
                .1
                .push(id);
        }
        let mut barrier = ReadyBarrier {
            sources: self.clone(),
            locks: Vec::new(),
        };
        for (graph, ids) in grouped.values() {
            for id in ids {
                if graph
                    .ready
                    .borrow()
                    .get(id)
                    .is_some_and(|count| *count == usize::MAX)
                {
                    return Err(RuntimeError::unavailable(
                        "reload_barrier",
                        "state barrier limit reached",
                    ));
                }
            }
        }
        for (graph, ids) in grouped.into_values() {
            let mut added = Vec::new();
            for id in ids {
                let mut locks = graph.ready.borrow_mut();
                let count = locks.entry(id).or_default();
                *count += 1; // Preflighted without callbacks or mutation between passes.
                added.push(id);
            }
            barrier.locks.push((graph, added));
        }
        *self.external.borrow_mut() = external;
        self.frozen.set(true);
        Ok(barrier)
    }
    fn external_value(&self, source: &Readable) -> Result<Rc<Value>> {
        if self.frozen.get()
            && let Some((_, value)) = self.external.borrow().get(&(source.port.as_ptr() as usize))
        {
            return Ok(value.clone());
        }
        source.snapshot().map(|snapshot| snapshot.shared_value())
    }
    pub(crate) fn authorize_initializer(
        self: &Rc<Self>,
        context: &Context,
        plan: &Rc<GraphReload>,
    ) -> Context {
        context.set_attachment(Rc::new(CandidateRead {
            sources: Rc::downgrade(self),
            plan: Rc::downgrade(plan),
        }));
        context.clone()
    }
    pub(crate) fn snapshot_in(source: &Readable, context: &Context) -> Result<Snapshot> {
        context.check(context.host_id())?;
        let Some(authority) = context.attachment::<CandidateRead>() else {
            return source.snapshot();
        };
        let sources = authority.sources.upgrade().ok_or_else(gone)?;
        authority.plan.upgrade().ok_or_else(gone)?;
        let Some((graph, id)) = &source.origin else {
            return source.snapshot();
        };
        let graph = graph.upgrade().ok_or_else(gone)?;
        let Some(plan) = sources
            .plans
            .iter()
            .find(|plan| Rc::ptr_eq(&plan.graph, &graph))
        else {
            return source.snapshot();
        };
        let value = sources.resolve(plan, *id, &mut Vec::new())?;
        Ok(Snapshot::normalized(value, graph.epoch.get()))
    }
    pub(crate) fn hydrate_external(
        self: &Rc<Self>,
        signal: CancellationToken,
    ) -> LocalBoxFuture<'static, Result<()>> {
        let this = self.clone();
        async move {
            let mut sources = Vec::new();
            for plan in &this.plans {
                for id in plan.owned.borrow().iter() {
                    let node = plan.graph.node(*id)?;
                    if let Kind::Import { source, .. } = &node.kind {
                        let source = this.project_source(source)?;
                        let candidate = source
                            .origin
                            .as_ref()
                            .and_then(|(graph, _)| graph.upgrade())
                            .is_some_and(|graph| {
                                this.plans.iter().any(|p| Rc::ptr_eq(&p.graph, &graph))
                            });
                        if !candidate {
                            sources.push(source.clone());
                        }
                    }
                }
            }
            for source in sources {
                signal.check()?;
                source.await_hydrated().await?;
            }
            Ok(())
        }
        .boxed_local()
    }
    pub(crate) fn prepare_all(&self) -> Result<Vec<PreparedGraphReload>> {
        self.prepare_migrations()?;
        let _projection = Projection::new(self);
        self.plans.iter().map(|plan| plan.prepare(self)).collect()
    }
    /// Called after incompatible owners have drained, before candidate initializers run.
    /// Successful private copies survive later projection passes without rerunning callbacks.
    pub(crate) fn prepare_migrations(&self) -> Result<()> {
        let mut graphs = BTreeMap::new();
        for plan in &self.plans {
            graphs
                .entry(Rc::as_ptr(&plan.graph) as usize)
                .or_insert_with(|| plan.graph.clone());
        }
        let _guards = graphs
            .into_values()
            .map(ReloadGuard::new)
            .collect::<Result<Vec<_>>>()?;
        self.cache.borrow_mut().clear();
        for plan in &self.plans {
            let changes = plan.cells.borrow().values().cloned().collect::<Vec<_>>();
            for change in changes.into_iter().filter(|change| change.changed) {
                let key = (Rc::as_ptr(&plan.graph) as usize, change.cell.node);
                if self.migrations.borrow().contains_key(&key) {
                    continue;
                }
                let old = plan
                    .graph
                    .node(change.cell.node)?
                    .port
                    .current()?
                    .shared_value();
                let value = match &change.migrate {
                    Some(migrate) => {
                        normalize(&migrate(old.as_ref().clone(), change.cell.version())?)?
                    }
                    None => old,
                };
                tembra_contracts::validate(&change.schema, &value)
                    .map_err(|error| RuntimeError::invalid("state", error.message))?;
                self.migrations.borrow_mut().insert(key, value);
            }
        }
        Ok(())
    }
    pub(super) fn resolve(
        &self,
        plan: &GraphReload,
        id: NodeId,
        stack: &mut Vec<(usize, NodeId)>,
    ) -> Result<Rc<Value>> {
        let key = (Rc::as_ptr(&plan.graph) as usize, id);
        if let Some(value) = self.migrations.borrow().get(&key).cloned() {
            return Ok(value);
        }
        if let Some(value) = self.cache.borrow().get(&key).cloned() {
            return Ok(value);
        }
        if stack.contains(&key) {
            return Err(RuntimeError::invalid(
                "state_cycle",
                "prospective state sources form a cycle",
            ));
        }
        stack.push(key);
        let node = plan.graph.node(id)?;
        let value = match &node.kind {
            Kind::Cell => node.port.current()?.shared_value(),
            Kind::Derived { sources, compute } => {
                let snapshots = sources
                    .iter()
                    .map(|id| {
                        self.resolve(plan, *id, stack)
                            .map(|value| Snapshot::normalized(value, plan.graph.epoch.get()))
                    })
                    .collect::<Result<Vec<_>>>()?;
                normalize(&compute(&snapshots)?)?
            }
            Kind::Import { source, .. } => {
                let source = self.project_source(source)?;
                if let Some((graph, id)) = &source.origin {
                    if let Some(other) = self
                        .plans
                        .iter()
                        .find(|p| Weak::ptr_eq(graph, &Rc::downgrade(&p.graph)))
                    {
                        self.resolve(other, *id, stack)?
                    } else {
                        self.external_value(&source)?
                    }
                } else {
                    self.external_value(&source)?
                }
            }
        };
        stack.pop();
        self.cache.borrow_mut().insert(key, value.clone());
        Ok(value)
    }
}
