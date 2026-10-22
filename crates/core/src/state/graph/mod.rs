mod activation;
mod mutation;
mod reload;
pub(crate) use reload::{
    CandidateSources, GraphDeclarationSet, GraphReload, PreparedGraphReload, ReadyBarrier,
};

use std::{
    cell::{Cell as Flag, RefCell},
    collections::{BTreeMap, HashMap},
    rc::{Rc, Weak},
};

use serde_json::Value;

use crate::{CancellationToken, ErrorCode, Result, RuntimeError};

use super::{
    MAX_REVISION, ReadGuard, ReadPort, Readable, Reporter, Snapshot, Subscription, WriteGuard,
    cell::{Cell, Initial, Metadata, StateDeclaration, StateInitializer},
    gone, normalize,
    publication::PublicationQueue,
};

pub(crate) type Derive = Rc<dyn Fn(&[Snapshot]) -> Result<Value>>;
pub(super) type NodeId = usize;

pub(crate) struct InitializerPlan {
    pub cell: Cell,
    pub initialize: StateInitializer,
}

enum Kind {
    Cell,
    Derived {
        sources: Vec<NodeId>,
        compute: Derive,
    },
    Import {
        source: Readable,
        subscription: RefCell<Option<Subscription>>,
    },
}

struct Node {
    port: Rc<ReadPort>,
    schema: RefCell<StateSchema>,
    installed: Rc<Flag<bool>>,
    constraints: RefCell<BTreeMap<u64, StateSchema>>,
    generation: Flag<u64>,
    unpublished: Flag<bool>,
    kind: Kind,
}

/// Keeps normalized public metadata and its immutable validation proof together.
#[derive(Clone)]
pub(crate) struct StateSchema {
    pub(crate) source: Rc<Value>,
    prepared: tembra_contracts::Result<tembra_contracts::PreparedSchema>,
}

impl std::ops::Deref for StateSchema {
    type Target = Value;

    fn deref(&self) -> &Value {
        &self.source
    }
}

impl StateSchema {
    pub(crate) fn validate(&self, value: &Value) -> tembra_contracts::Result<()> {
        match &self.prepared {
            Ok(prepared) => prepared.validate(value),
            Err(error) => {
                tembra_contracts::validate(&Value::Bool(true), value)?;
                Err(error.clone())
            }
        }
    }
}

pub(super) struct Candidate {
    pub node: NodeId,
    pub value: Rc<Value>,
}
pub(super) struct ReadStamp {
    node: NodeId,
    revision: Option<u64>,
    generation: u64,
}

/// Object-owned graph. Public capabilities only retain weak graph/port links.
pub(crate) struct StateGraph {
    nodes: RefCell<Vec<Option<Rc<Node>>>>,
    named: RefCell<HashMap<String, Cell>>,
    initializers: RefCell<Vec<InitializerPlan>>,
    read_guard: ReadGuard,
    subscribe_guard: ReadGuard,
    write_guard: WriteGuard,
    reporter: Reporter,
    queue: Rc<PublicationQueue>,
    topology: Flag<u64>,
    epoch: Flag<u64>,
    exposure: Flag<u64>,
    preparing: Flag<bool>,
    pending_refreshes: RefCell<BTreeMap<NodeId, bool>>,
    refreshing: Flag<bool>,
    ready: RefCell<BTreeMap<NodeId, usize>>,
    deferred_retirements: RefCell<Vec<reload::DeferredRetirement>>,
    active: Flag<bool>,
    ended: RefCell<Option<RuntimeError>>,
    stopped: CancellationToken,
}

impl StateGraph {
    pub fn new(
        read_guard: ReadGuard,
        subscribe_guard: ReadGuard,
        write_guard: WriteGuard,
        reporter: Reporter,
        queue: Rc<PublicationQueue>,
    ) -> Rc<Self> {
        Rc::new(Self {
            nodes: RefCell::default(),
            named: RefCell::default(),
            initializers: RefCell::default(),
            read_guard,
            subscribe_guard,
            write_guard,
            reporter,
            queue,
            topology: Flag::new(0),
            epoch: Flag::new(0),
            exposure: Flag::new(0),
            preparing: Flag::new(false),
            pending_refreshes: RefCell::default(),
            refreshing: Flag::new(false),
            ready: RefCell::default(),
            deferred_retirements: RefCell::default(),
            active: Flag::new(false),
            ended: RefCell::default(),
            stopped: CancellationToken::new(),
        })
    }

    fn check(&self) -> Result<()> {
        if let Some(error) = &*self.ended.borrow() {
            return Err(error.clone());
        }
        if self.preparing.get() {
            return Err(RuntimeError::invalid(
                "state_reentrancy",
                "use the supplied staged snapshots during preparation",
            ));
        }
        self.flush_refreshes();
        Ok(())
    }

    fn port(self: &Rc<Self>) -> Rc<ReadPort> {
        let read = Rc::downgrade(self);
        let subscribe = read.clone();
        ReadPort::new(
            Rc::new(move || {
                let graph = read.upgrade().ok_or_else(gone)?;
                graph.check()?;
                (graph.read_guard)()
            }),
            Rc::new(move || {
                let graph = subscribe.upgrade().ok_or_else(gone)?;
                graph.check()?;
                (graph.subscribe_guard)()
            }),
            self.reporter.clone(),
            self.queue.clone(),
        )
    }

    fn node(&self, id: NodeId) -> Result<Rc<Node>> {
        self.nodes
            .borrow()
            .get(id)
            .and_then(Clone::clone)
            .ok_or_else(gone)
    }
    fn check_ready(&self, id: NodeId) -> Result<()> {
        if self.ready.borrow().contains_key(&id) {
            return Err(RuntimeError::new(
                ErrorCode::UpgradeConflict,
                "upgrade_conflict",
                "state is in a reload commit barrier",
            ));
        }
        Ok(())
    }

    pub fn owns(self: &Rc<Self>, source: &Readable) -> bool {
        source
            .origin
            .as_ref()
            .is_some_and(|(graph, _)| Weak::ptr_eq(graph, &Rc::downgrade(self)))
    }

    fn source_id(self: &Rc<Self>, source: &Readable) -> Result<NodeId> {
        if !self.owns(source) {
            return Err(RuntimeError::invalid(
                "foreign_state",
                "source belongs to another graph",
            ));
        }
        Ok(source.origin.as_ref().expect("owned source has origin").1)
    }

    pub(crate) fn schema(schema: Value) -> Result<StateSchema> {
        let source = tembra_contracts::normalize_schema(&schema)
            .map_err(|error| RuntimeError::invalid(error.reason, error.message))?;
        let prepared = tembra_contracts::PreparedSchema::new(&source);
        Ok(StateSchema {
            source: Rc::new(source),
            prepared,
        })
    }

    fn declaration_schema(schema: super::cell::DeclarationSchema) -> Result<StateSchema> {
        match schema {
            super::cell::DeclarationSchema::Dynamic(value) => Self::schema(value),
            super::cell::DeclarationSchema::Prepared(definition) => {
                Ok(definition.inner.as_ref().clone())
            }
        }
    }

    fn validate(node: &Node, value: &Value) -> Result<()> {
        let schemas: Vec<_> = std::iter::once(node.schema.borrow().clone())
            .chain(node.constraints.borrow().values().cloned())
            .collect();
        for schema in schemas {
            schema
                .validate(value)
                .map_err(|error| RuntimeError::invalid("state", error.message))?;
        }
        Ok(())
    }

    pub fn declare_cell(
        self: &Rc<Self>,
        name: String,
        declaration: StateDeclaration,
    ) -> Result<Cell> {
        self.check()?;
        if name.is_empty() || self.named.borrow().contains_key(&name) {
            return Err(RuntimeError::invalid(
                "duplicate",
                "state names must be nonempty and unique",
            ));
        }
        if declaration.version == 0 || declaration.version > MAX_REVISION {
            return Err(RuntimeError::invalid(
                "state_version",
                "state version must be a positive safe integer",
            ));
        }
        let schema = Self::declaration_schema(declaration.schema)?;
        let port = self.port();
        let node = Rc::new(Node {
            port: port.clone(),
            schema: RefCell::new(schema.clone()),
            installed: Rc::new(Flag::new(true)),
            constraints: RefCell::default(),
            generation: Flag::new(0),
            unpublished: Flag::new(false),
            kind: Kind::Cell,
        });
        let initial = match declaration.initial {
            Initial::Value(value) => {
                let value = normalize(&value)?;
                Self::validate(&node, &value)?;
                port.seed(Snapshot::normalized(value, self.epoch.get()));
                None
            }
            Initial::Async(initialize) => Some(initialize),
        };
        let id = self.nodes.borrow().len();
        let cell = Cell {
            graph: Rc::downgrade(self),
            node: id,
            metadata: Rc::new(Metadata {
                name: name.clone(),
                schema: RefCell::new(schema.source),
                version: Flag::new(declaration.version),
            }),
            readable: port.node_readable(self, id),
        };
        self.nodes.borrow_mut().push(Some(node));
        self.named.borrow_mut().insert(name, cell.clone());
        if let Some(initialize) = initial {
            self.initializers.borrow_mut().push(InitializerPlan {
                cell: cell.clone(),
                initialize,
            });
        }
        self.topology.set(self.topology.get() + 1);
        Ok(cell)
    }

    pub fn take_initializers(&self) -> Vec<InitializerPlan> {
        std::mem::take(&mut *self.initializers.borrow_mut())
    }

    pub fn forward(self: &Rc<Self>, source: Readable) -> Result<Readable> {
        self.check()?;
        if self.owns(&source) {
            return Ok(source);
        }
        let existing = self
            .nodes
            .borrow()
            .iter()
            .enumerate()
            .find_map(|(id, node)| {
                let node = node.as_ref()?;
                if !node.installed.get() {
                    return None;
                }
                if let Kind::Import {
                    source: existing, ..
                } = &node.kind
                {
                    existing
                        .same_source(&source)
                        .then(|| node.port.node_readable(self, id))
                } else {
                    None
                }
            });
        if let Some(existing) = existing {
            return Ok(existing);
        }
        let port = self.port();
        match source.snapshot() {
            Ok(snapshot) => port.seed(Snapshot::new(snapshot.shared_value(), self.epoch.get())?),
            Err(error) if error.code == ErrorCode::NotReady => {}
            Err(error) => return Err(error),
        }
        let id = self.nodes.borrow().len();
        self.nodes.borrow_mut().push(Some(Rc::new(Node {
            port: port.clone(),
            schema: RefCell::new(Self::schema(Value::Bool(true))?),
            installed: Rc::new(Flag::new(true)),
            constraints: RefCell::default(),
            generation: Flag::new(0),
            unpublished: Flag::new(false),
            kind: Kind::Import {
                source,
                subscription: RefCell::default(),
            },
        })));
        self.topology.set(self.topology.get() + 1);
        Ok(port.node_readable(self, id))
    }

    pub fn derive(
        self: &Rc<Self>,
        sources: Vec<Readable>,
        schema: Value,
        compute: Derive,
    ) -> Result<Readable> {
        self.derive_with(sources, || Self::schema(schema), compute)
    }

    pub(crate) fn derive_definition(
        self: &Rc<Self>,
        sources: Vec<Readable>,
        definition: super::StateDefinition,
        compute: Derive,
    ) -> Result<Readable> {
        self.derive_with(sources, || Ok(definition.inner.as_ref().clone()), compute)
    }

    fn derive_with(
        self: &Rc<Self>,
        sources: Vec<Readable>,
        prepare: impl FnOnce() -> Result<StateSchema>,
        compute: Derive,
    ) -> Result<Readable> {
        self.check()?;
        let schema = prepare()?;
        let mut ids = Vec::new();
        for source in sources {
            ids.push(self.source_id(&self.forward(source)?)?);
        }
        let port = self.port();
        let node = Rc::new(Node {
            port: port.clone(),
            schema: RefCell::new(schema),
            installed: Rc::new(Flag::new(true)),
            constraints: RefCell::default(),
            generation: Flag::new(0),
            unpublished: Flag::new(false),
            kind: Kind::Derived {
                sources: ids,
                compute,
            },
        });
        let mut preparation = None;
        if let Kind::Derived { sources, compute } = &node.kind {
            let snapshots: Result<Vec<_>> = sources
                .iter()
                .map(|id| self.node(*id)?.port.current())
                .collect();
            match snapshots {
                Ok(snapshots) => {
                    preparation = Some(Preparing::new(self)?);
                    let value = normalize(&compute(&snapshots)?)?;
                    Self::validate(&node, &value)?;
                    port.seed(Snapshot::normalized(value, self.epoch.get()));
                }
                Err(error) if error.code == ErrorCode::NotReady => {}
                Err(error) => return Err(error),
            }
        }
        let id = self.nodes.borrow().len();
        self.nodes.borrow_mut().push(Some(node));
        self.topology.set(self.topology.get() + 1);
        drop(preparation);
        Ok(port.node_readable(self, id))
    }

    pub(crate) fn expose_definition(
        self: &Rc<Self>,
        source: &Readable,
        definition: Result<super::StateDefinition>,
    ) -> Result<Exposure> {
        self.expose_with(source, || {
            definition.map(|value| value.inner.as_ref().clone())
        })
    }

    fn expose_with(
        self: &Rc<Self>,
        source: &Readable,
        prepare: impl FnOnce() -> Result<StateSchema>,
    ) -> Result<Exposure> {
        self.check()?;
        let id = self.source_id(source)?;
        let node = self.node(id)?;
        let schema = prepare()?;
        match node.port.current() {
            Ok(snapshot) => schema
                .validate(snapshot.value())
                .map_err(|error| RuntimeError::invalid("state", error.message))?,
            Err(error) if error.code == ErrorCode::NotReady => {}
            Err(error) => return Err(error),
        }
        let key = self.exposure.get() + 1;
        self.exposure.set(key);
        node.constraints.borrow_mut().insert(key, schema);
        self.topology.set(self.topology.get() + 1);
        Ok(Exposure {
            graph: Rc::downgrade(self),
            node: id,
            key,
        })
    }
}

pub(crate) struct Exposure {
    graph: Weak<StateGraph>,
    node: NodeId,
    key: u64,
}

impl Drop for Exposure {
    fn drop(&mut self) {
        if let Some(graph) = self.graph.upgrade()
            && let Ok(node) = graph.node(self.node)
        {
            let schema = node.constraints.borrow_mut().remove(&self.key);
            graph.topology.set(graph.topology.get() + 1);
            drop(schema);
        }
    }
}

impl Drop for StateGraph {
    fn drop(&mut self) {
        self.stopped.cancel(gone());
        for node in self.nodes.get_mut().iter().flatten() {
            node.port.terminate(gone());
        }
    }
}

struct Preparing<'a>(&'a StateGraph);
impl<'a> Preparing<'a> {
    fn new(graph: &'a StateGraph) -> Result<Self> {
        if graph.preparing.replace(true) {
            return Err(RuntimeError::invalid(
                "state_reentrancy",
                "a state candidate is already being prepared",
            ));
        }
        Ok(Self(graph))
    }
}
impl Drop for Preparing<'_> {
    fn drop(&mut self) {
        self.0.preparing.set(false);
        // Native panic propagates. Defer user computation until a later safe entry
        // rather than risking a second panic while already unwinding.
        self.0.flush_refreshes();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn state_schema_keeps_second_pass_refusal_deferred_and_value_error_precedence() {
        let properties = (0..9_000)
            .map(|index| (format!("a{index}"), json!({"type":"array"})))
            .collect::<serde_json::Map<_, _>>();
        let source = json!({"type":"object", "properties":properties});
        let schema = StateGraph::schema(source).unwrap();
        assert_eq!(
            schema.prepared.as_ref().unwrap_err().reason,
            "schema_budget"
        );
        for value in [json!({}), json!(9_007_199_254_740_992_u64)] {
            assert_eq!(
                schema.validate(&value),
                tembra_contracts::validate(&schema.source, &value)
            );
        }
        let valid = StateGraph::schema(json!({"type":"integer"})).unwrap();
        assert!(valid.validate(&json!("bad")).is_err());
        valid.clone().validate(&json!(1)).unwrap();
        assert!(StateGraph::schema(json!({"type":"unknown"})).is_err());
        valid.validate(&json!(2)).unwrap();
    }
}
