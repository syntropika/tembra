use super::super::{StateEvent, publication::PendingPublication};
use super::*;
use crate::Context;
use crate::state::cell::StateMigration;

mod declarations;
mod sources;
pub(super) use declarations::DeferredRetirement;
pub(crate) use declarations::GraphDeclarationSet;
use sources::Projection;
pub(crate) use sources::{CandidateSources, ReadyBarrier};

struct CellChange {
    cell: Cell,
    schema: StateSchema,
    version: u64,
    migrate: Option<StateMigration>,
    changed: bool,
}
pub(crate) struct GraphReload {
    graph: Rc<StateGraph>,
    projection_only: bool,
    baseline: GraphDeclarationSet,
    read: ReadGuard,
    cells: RefCell<BTreeMap<String, Rc<CellChange>>>,
    reserved: RefCell<Vec<NodeId>>,
    owned: RefCell<Vec<NodeId>>,
    exposures: RefCell<Vec<(NodeId, u64, StateSchema)>>,
    initializers: RefCell<Vec<InitializerPlan>>,
    sealed: Flag<bool>,
    adopted: Flag<bool>,
    aborted: Flag<bool>,
}
impl GraphReload {
    pub(crate) fn declarations(&self) -> GraphDeclarationSet {
        if self.projection_only {
            return self.baseline.clone();
        }
        GraphDeclarationSet {
            graph: Rc::downgrade(&self.graph),
            nodes: Rc::new(self.owned.borrow().clone()),
            exposures: Rc::new(
                self.exposures
                    .borrow()
                    .iter()
                    .map(|(id, key, _)| (*id, *key))
                    .collect(),
            ),
        }
    }
    fn check(&self) -> Result<()> {
        if self.aborted.get() || self.adopted.get() || self.sealed.get() {
            return Err(RuntimeError::invalid(
                "declaration_scope",
                "candidate state recording ended",
            ));
        }
        self.graph.check()
    }
    fn reserve(&self, kind: Kind, schema: StateSchema) -> Result<(NodeId, Rc<Node>)> {
        let installed = Rc::new(Flag::new(false));
        let flag = installed.clone();
        let read = self.read.clone();
        let weak = Rc::downgrade(&self.graph);
        let read_guard = Rc::new(move || {
            let graph = weak.upgrade().ok_or_else(gone)?;
            graph.check()?;
            if flag.get() {
                (graph.read_guard)()
            } else {
                read()
            }
        });
        let subscribed = installed.clone();
        let owner = Rc::downgrade(&self.graph);
        let subscribe = Rc::new(move || {
            let graph = owner.upgrade().ok_or_else(gone)?;
            if !subscribed.get() {
                return Err(RuntimeError::invalid(
                    "declaration_scope",
                    "candidate state is not installed",
                ));
            }
            graph.check()?;
            (graph.subscribe_guard)()
        });
        let port = ReadPort::new(
            read_guard,
            subscribe,
            self.graph.reporter.clone(),
            self.graph.queue.clone(),
        );
        let node = Rc::new(Node {
            port,
            schema: RefCell::new(schema),
            installed,
            constraints: RefCell::default(),
            generation: Flag::new(0),
            unpublished: Flag::new(false),
            kind,
        });
        let id = self.graph.nodes.borrow().len();
        self.graph.nodes.borrow_mut().push(Some(node.clone()));
        self.reserved.borrow_mut().push(id);
        self.owned.borrow_mut().push(id);
        Ok((id, node))
    }
    pub(crate) fn declare_cell(&self, name: String, declaration: StateDeclaration) -> Result<Cell> {
        self.check()?;
        if name.is_empty() || self.cells.borrow().contains_key(&name) {
            return Err(RuntimeError::invalid(
                "duplicate",
                "state names must be nonempty and unique",
            ));
        }
        let schema = StateGraph::declaration_schema(declaration.schema.clone())?;
        let existing = self.graph.named.borrow().get(&name).cloned();
        let (cell, changed) = if let Some(cell) = existing {
            let changed = cell.compare_declaration(&schema, declaration.version)?
                == super::super::StateMetadataChange::MigrationRequired;
            self.owned.borrow_mut().push(cell.node);
            (cell, changed)
        } else {
            if declaration.version == 0 || declaration.version > MAX_REVISION {
                return Err(RuntimeError::invalid(
                    "state_version",
                    "state version must be a positive safe integer",
                ));
            }
            let (id, node) = self.reserve(Kind::Cell, schema.clone())?;
            let cell = Cell {
                graph: Rc::downgrade(&self.graph),
                node: id,
                metadata: Rc::new(Metadata {
                    name: name.clone(),
                    schema: RefCell::new(schema.source.clone()),
                    version: Flag::new(declaration.version),
                }),
                readable: node.port.node_readable(&self.graph, id),
            };
            match declaration.initial {
                Initial::Value(value) => {
                    let value = normalize(&value)?;
                    StateGraph::validate(&node, &value)?;
                    node.port
                        .seed(Snapshot::normalized(value, self.graph.epoch.get()));
                }
                Initial::Async(initialize) => {
                    self.initializers.borrow_mut().push(InitializerPlan {
                        cell: cell.clone(),
                        initialize,
                    })
                }
            }
            (cell, false)
        };
        self.cells.borrow_mut().insert(
            name,
            Rc::new(CellChange {
                cell: cell.clone(),
                schema,
                version: declaration.version,
                migrate: declaration.migrate,
                changed,
            }),
        );
        Ok(cell)
    }
    fn source(&self, source: Readable) -> Result<NodeId> {
        if self.graph.owns(&source) {
            return self.graph.source_id(&source);
        }
        let nodes = self.owned.borrow().clone();
        for id in nodes {
            let node = self.graph.node(id)?;
            if let Kind::Import { source: old, .. } = &node.kind
                && old.same_source(&source)
            {
                return Ok(id);
            }
        }
        let (id, _) = self.reserve(
            Kind::Import {
                source,
                subscription: RefCell::default(),
            },
            StateGraph::schema(Value::Bool(true))?,
        )?;
        Ok(id)
    }
    pub(crate) fn forward(&self, source: Readable) -> Result<Readable> {
        self.check()?;
        let id = self.source(source)?;
        Ok(self.graph.node(id)?.port.node_readable(&self.graph, id))
    }
    pub(crate) fn derive(
        &self,
        sources: Vec<Readable>,
        schema: Value,
        compute: Derive,
    ) -> Result<Readable> {
        self.derive_with(sources, || StateGraph::schema(schema), compute)
    }

    pub(crate) fn derive_definition(
        &self,
        sources: Vec<Readable>,
        definition: super::super::StateDefinition,
        compute: Derive,
    ) -> Result<Readable> {
        self.derive_with(sources, || Ok(definition.inner.as_ref().clone()), compute)
    }

    fn derive_with(
        &self,
        sources: Vec<Readable>,
        prepare: impl FnOnce() -> Result<StateSchema>,
        compute: Derive,
    ) -> Result<Readable> {
        self.check()?;
        let schema = prepare()?;
        let ids = sources
            .into_iter()
            .map(|source| self.source(source))
            .collect::<Result<Vec<_>>>()?;
        let (id, node) = self.reserve(
            Kind::Derived {
                sources: ids,
                compute,
            },
            schema,
        )?;
        if let Kind::Derived { sources, compute } = &node.kind {
            let snapshots = sources
                .iter()
                .map(|id| self.graph.node(*id)?.port.current())
                .collect::<Result<Vec<_>>>();
            match snapshots {
                Ok(snapshots) => {
                    let _guard = Preparing::new(&self.graph)?;
                    let value = normalize(&compute(&snapshots)?)?;
                    StateGraph::validate(&node, &value)?;
                    node.port
                        .seed(Snapshot::normalized(value, self.graph.epoch.get()));
                }
                Err(error) if error.code == ErrorCode::NotReady => {}
                Err(error) => return Err(error),
            }
        }
        Ok(node.port.node_readable(&self.graph, id))
    }
    pub(crate) fn expose(&self, source: &Readable, schema: Value) -> Result<()> {
        self.check()?;
        let id = self.graph.source_id(source)?;
        let schema = StateGraph::schema(schema)?;
        let key = self.graph.exposure.get() + 1;
        self.graph.exposure.set(key);
        self.exposures.borrow_mut().push((id, key, schema));
        Ok(())
    }
    pub(crate) fn expose_definition(
        &self,
        source: &Readable,
        definition: Result<super::super::StateDefinition>,
    ) -> Result<()> {
        self.check()?;
        let id = self.graph.source_id(source)?;
        let schema = definition?.inner.as_ref().clone();
        let key = self.graph.exposure.get() + 1;
        self.graph.exposure.set(key);
        self.exposures.borrow_mut().push((id, key, schema));
        Ok(())
    }

    pub(crate) fn take_initializers(&self) -> Vec<InitializerPlan> {
        std::mem::take(&mut *self.initializers.borrow_mut())
    }
    pub(crate) fn initialize(&self, results: Vec<(Cell, Value)>) -> Result<()> {
        let mut seeds = Vec::new();
        for (cell, value) in results {
            if !self.reserved.borrow().contains(&cell.node) || !self.graph.owns(&cell.readable) {
                return Err(RuntimeError::invalid(
                    "foreign_state",
                    "initializer does not own this candidate cell",
                ));
            }
            let node = self.graph.node(cell.node)?;
            if node.port.current().is_ok() || seeds.iter().any(|(id, _)| *id == cell.node) {
                return Err(RuntimeError::invalid(
                    "state_initialized",
                    "candidate cell is already initialized",
                ));
            }
            let value = normalize(&value)?;
            StateGraph::validate(&node, &value)?;
            seeds.push((cell.node, value));
        }
        for (id, value) in seeds {
            self.graph
                .node(id)?
                .port
                .seed(Snapshot::normalized(value, self.graph.epoch.get()));
        }
        Ok(())
    }
    pub(crate) fn requires_gate(&self) -> bool {
        self.cells.borrow().values().any(|cell| cell.changed)
    }
    pub(crate) fn prepare(
        self: &Rc<Self>,
        sources: &CandidateSources,
    ) -> Result<PreparedGraphReload> {
        self.sealed.set(true);
        let _projection = Projection::new(sources);
        let guard = ReloadGuard::new(self.graph.clone())?;
        let cells = self.cells.borrow().values().cloned().collect::<Vec<_>>();
        let mut candidates = BTreeMap::new();
        let ids = if self.projection_only {
            self.baseline.nodes.as_ref().clone()
        } else if self.requires_gate() {
            self.owned.borrow().clone()
        } else {
            self.graph
                .nodes
                .borrow()
                .iter()
                .enumerate()
                .filter_map(|(id, node)| {
                    node.as_ref()
                        .filter(|node| node.installed.get() || self.owned.borrow().contains(&id))
                        .map(|_| id)
                })
                .collect()
        };
        for id in ids {
            candidates.insert(id, sources.resolve(self, id, &mut Vec::new())?);
        }
        for (id, _, schema) in self.exposures.borrow().iter() {
            let value = candidates.get(id).ok_or_else(gone)?;
            schema
                .validate(value)
                .map_err(|error| RuntimeError::invalid("state", error.message))?;
        }
        let next = self
            .graph
            .epoch
            .get()
            .checked_add(1)
            .filter(|n| *n <= MAX_REVISION)
            .ok_or_else(|| {
                RuntimeError::unavailable("state_revision", "state revision space exhausted")
            })?;
        let mut events = Vec::new();
        for (id, value) in candidates {
            let node = self.graph.node(id)?;
            let schema = cells
                .iter()
                .find(|change| change.cell.node == id)
                .map(|change| change.schema.clone())
                .unwrap_or_else(|| node.schema.borrow().clone());
            schema
                .validate(&value)
                .map_err(|error| RuntimeError::invalid("state", error.message))?;
            {
                for (key, schema) in node.constraints.borrow().iter() {
                    if self.projection_only && !self.baseline.exposures.contains(&(id, *key)) {
                        continue;
                    }
                    if self.requires_gate() && self.baseline.exposures.contains(&(id, *key)) {
                        continue;
                    }
                    schema
                        .validate(&value)
                        .map_err(|error| RuntimeError::invalid("state", error.message))?;
                }
            }
            let snapshot = Snapshot::normalized(value, next);
            if !node.installed.get() {
                events.push((node, StateEvent::Snapshot(snapshot)));
            } else if cells
                .iter()
                .any(|change| change.cell.node == id && change.changed)
                || node.port.current()?.value() != snapshot.value()
            {
                events.push((node, StateEvent::Reset(snapshot)));
            }
        }
        Ok(PreparedGraphReload {
            plan: self.clone(),
            guard: Some(guard),
            topology: self.graph.topology.get(),
            epoch: self.graph.epoch.get(),
            next,
            events: Some(events),
            retired: None,
        })
    }
    pub(crate) fn abort(&self) -> RetiredGraphDeclarations {
        if self.adopted.get() {
            return RetiredGraphDeclarations::default();
        }
        self.aborted.set(true);
        let mut nodes = Vec::new();
        for id in self.reserved.borrow().iter() {
            if let Some(node) = self
                .graph
                .nodes
                .borrow_mut()
                .get_mut(*id)
                .and_then(Option::take)
            {
                self.graph.pending_refreshes.borrow_mut().remove(id);
                nodes.push(node);
            }
        }
        RetiredGraphDeclarations {
            nodes,
            schemas: Vec::new(),
            metadata_schemas: Vec::new(),
        }
    }
}
struct ReloadGuard(Rc<StateGraph>);
impl ReloadGuard {
    fn new(graph: Rc<StateGraph>) -> Result<Self> {
        if graph.preparing.replace(true) {
            return Err(RuntimeError::invalid(
                "state_reentrancy",
                "state preparation is active",
            ));
        }
        Ok(Self(graph))
    }
}
impl Drop for ReloadGuard {
    fn drop(&mut self) {
        self.0.preparing.set(false);
        self.0.flush_refreshes();
    }
}

pub(crate) struct PreparedGraphReload {
    plan: Rc<GraphReload>,
    guard: Option<ReloadGuard>,
    topology: u64,
    epoch: u64,
    next: u64,
    events: Option<Vec<(Rc<Node>, StateEvent)>>,
    retired: Option<RetiredGraphDeclarations>,
}
impl PreparedGraphReload {
    pub(crate) fn graph(&self) -> &Rc<StateGraph> {
        &self.plan.graph
    }
    /// Releases synchronous computation admission while a separately owned ready barrier
    /// preserves the prepared read set across a coordinator wait.
    pub(crate) fn release_preparing(&mut self) {
        drop(self.guard.take());
    }
    pub(crate) fn preflight(&self) -> Result<()> {
        if let Some(error) = self.plan.graph.ended.borrow().clone() {
            return Err(error);
        }
        if self
            .events
            .as_ref()
            .is_some_and(|events| events.iter().any(|(node, _)| node.port.is_terminated()))
        {
            return Err(gone());
        }
        if self.plan.graph.topology.get() != self.topology
            || self.plan.graph.epoch.get() != self.epoch
            || self.plan.aborted.get()
            || self.events.is_none()
        {
            return Err(RuntimeError::invalid(
                "stale_state",
                "candidate graph changed",
            ));
        }
        Ok(())
    }
    pub(crate) fn adopt(&mut self) -> Vec<PendingPublication> {
        let graph = &self.plan.graph;
        let mut schemas = Vec::new();
        let mut metadata_schemas = Vec::new();
        let mut removed = Vec::new();
        if self.plan.requires_gate() {
            for id in self.plan.baseline.nodes.iter() {
                if !self.plan.owned.borrow().contains(id)
                    && let Some(node) = graph.nodes.borrow_mut().get_mut(*id).and_then(Option::take)
                {
                    graph.pending_refreshes.borrow_mut().remove(id);
                    removed.push(node);
                }
            }
            for (id, key) in self.plan.baseline.exposures.iter() {
                if let Ok(node) = graph.node(*id)
                    && let Some(schema) = node.constraints.borrow_mut().remove(key)
                {
                    schemas.push(schema);
                }
            }
        }
        for change in self.plan.cells.borrow().values() {
            let node = graph
                .node(change.cell.node)
                .expect("preflighted candidate node");
            schemas.push(node.schema.replace(change.schema.clone()));
            metadata_schemas.push(
                change
                    .cell
                    .metadata
                    .schema
                    .replace(change.schema.source.clone()),
            );
            change.cell.metadata.version.set(change.version);
            graph
                .named
                .borrow_mut()
                .insert(change.cell.name().to_owned(), change.cell.clone());
        }
        for (id, key, schema) in self.plan.exposures.borrow().iter() {
            graph
                .node(*id)
                .expect("preflighted exposure node")
                .constraints
                .borrow_mut()
                .insert(*key, schema.clone());
        }
        for id in self.plan.owned.borrow().iter() {
            graph
                .node(*id)
                .expect("candidate node exists")
                .installed
                .set(true);
        }
        let events = self.events.take().expect("candidate graph adopted once");
        if !events.is_empty() {
            graph.epoch.set(self.next);
        }
        graph.active.set(true);
        if !self.plan.projection_only {
            graph.topology.set(graph.topology.get() + 1);
        }
        self.plan.adopted.set(true);
        self.retired = Some(RetiredGraphDeclarations {
            nodes: removed,
            schemas,
            metadata_schemas,
        });
        events
            .into_iter()
            .filter_map(|(node, event)| {
                node.generation.set(node.generation.get() + 1);
                node.port.adopt(event)
            })
            .collect()
    }
    pub(crate) fn finish(mut self) {
        drop(self.guard.take());
        if let Some(retired) = self.retired.take() {
            retired.finish();
        }
        let ids = self.plan.owned.borrow().clone();
        for id in ids {
            let Ok(node) = self.plan.graph.node(id) else {
                continue;
            };
            if let Kind::Import {
                source,
                subscription,
            } = &node.kind
            {
                if subscription.borrow().is_some() {
                    continue;
                }
                let graph = Rc::downgrade(&self.plan.graph);
                let attached = source.subscribe_events(move |event| {
                    if let Some(graph) = graph.upgrade()
                        && let Err(error) = graph.refresh(id, event)
                    {
                        (graph.reporter)(error);
                    }
                    Ok(())
                });
                match attached {
                    Ok(attached) => *subscription.borrow_mut() = Some(attached),
                    Err(error) => (self.plan.graph.reporter)(error),
                }
            }
        }
    }
}
#[derive(Default)]
pub(crate) struct RetiredGraphDeclarations {
    nodes: Vec<Rc<Node>>,
    schemas: Vec<StateSchema>,
    metadata_schemas: Vec<Rc<Value>>,
}
impl RetiredGraphDeclarations {
    pub(crate) fn finish(self) {
        for node in &self.nodes {
            node.port.terminate(gone());
        }
        drop(self.schemas);
        drop(self.metadata_schemas);
        drop(self.nodes);
    }
}

#[cfg(test)]
mod tests;
