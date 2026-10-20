use super::*;

#[derive(Clone)]
pub(crate) struct GraphDeclarationSet {
    pub(super) graph: Weak<StateGraph>,
    pub(super) nodes: Rc<Vec<NodeId>>,
    pub(super) exposures: Rc<Vec<(NodeId, u64)>>,
}
pub(in crate::state::graph) struct DeferredRetirement {
    declarations: GraphDeclarationSet,
    retained: Vec<GraphDeclarationSet>,
}
impl GraphDeclarationSet {
    pub(crate) fn owns(&self, source: &Readable) -> bool {
        source
            .origin
            .as_ref()
            .is_some_and(|(graph, id)| Weak::ptr_eq(graph, &self.graph) && self.nodes.contains(id))
    }
    pub(crate) fn subscribe(
        &self,
        source: &Readable,
        callback: Rc<dyn Fn(Snapshot) -> Result<()>>,
    ) -> Result<Subscription> {
        if !self.owns(source) {
            return Err(RuntimeError::invalid(
                "foreign_state",
                "source is not in this declaration set",
            ));
        }
        let graph = self.graph.upgrade().ok_or_else(gone)?;
        let id = source.origin.as_ref().expect("owned source origin").1;
        let installed = graph.node(id)?.installed.get();
        source.port.upgrade().ok_or_else(gone)?.subscribe_staged(
            Rc::new(move |event| callback(event.snapshot().clone())),
            installed,
        )
    }
}

impl StateGraph {
    pub(crate) fn declarations(self: &Rc<Self>) -> GraphDeclarationSet {
        let nodes = self.nodes.borrow();
        GraphDeclarationSet {
            graph: Rc::downgrade(self),
            nodes: Rc::new(
                nodes
                    .iter()
                    .enumerate()
                    .filter_map(|(id, node)| {
                        node.as_ref()
                            .filter(|node| node.installed.get())
                            .map(|_| id)
                    })
                    .collect(),
            ),
            exposures: Rc::new(
                nodes
                    .iter()
                    .enumerate()
                    .flat_map(|(id, node)| {
                        node.as_ref()
                            .map(|node| {
                                node.constraints
                                    .borrow()
                                    .keys()
                                    .map(|key| (id, *key))
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default()
                    })
                    .collect(),
            ),
        }
    }
    pub(crate) fn begin_reload(self: &Rc<Self>, read: ReadGuard) -> Result<Rc<GraphReload>> {
        self.check()?;
        Ok(Rc::new(GraphReload {
            graph: self.clone(),
            projection_only: false,
            baseline: self.declarations(),
            read,
            cells: RefCell::default(),
            reserved: RefCell::default(),
            owned: RefCell::default(),
            exposures: RefCell::default(),
            initializers: RefCell::default(),
            sealed: Flag::new(false),
            adopted: Flag::new(false),
            aborted: Flag::new(false),
        }))
    }
    pub(crate) fn retire(
        self: &Rc<Self>,
        declarations: &GraphDeclarationSet,
        retained: &[GraphDeclarationSet],
    ) -> RetiredGraphDeclarations {
        if !Weak::ptr_eq(&declarations.graph, &Rc::downgrade(self)) {
            return RetiredGraphDeclarations::default();
        }
        let keeps_node = |id: &NodeId| {
            retained
                .iter()
                .any(|set| Weak::ptr_eq(&set.graph, &declarations.graph) && set.nodes.contains(id))
        };
        let keeps_exposure = |entry: &(NodeId, u64)| {
            retained.iter().any(|set| {
                Weak::ptr_eq(&set.graph, &declarations.graph) && set.exposures.contains(entry)
            })
        };
        let changes_nodes = declarations
            .nodes
            .iter()
            .any(|id| !keeps_node(id) && self.nodes.borrow().get(*id).is_some_and(Option::is_some));
        let changes_constraints = declarations.exposures.iter().any(|entry| {
            !keeps_exposure(entry)
                && self
                    .node(entry.0)
                    .is_ok_and(|node| node.constraints.borrow().contains_key(&entry.1))
        });
        if !changes_nodes && !changes_constraints {
            return RetiredGraphDeclarations::default();
        }
        if !self.ready.borrow().is_empty() {
            self.deferred_retirements
                .borrow_mut()
                .push(DeferredRetirement {
                    declarations: declarations.clone(),
                    retained: retained.to_vec(),
                });
            return RetiredGraphDeclarations::default();
        }
        let mut removed = Vec::new();
        let mut schemas = Vec::new();
        for (id, key) in declarations.exposures.iter() {
            if !keeps_exposure(&(*id, *key))
                && let Ok(node) = self.node(*id)
                && let Some(schema) = node.constraints.borrow_mut().remove(key)
            {
                schemas.push(schema);
            }
        }
        for id in declarations.nodes.iter() {
            if !keeps_node(id) {
                let node = self.nodes.borrow_mut().get_mut(*id).and_then(Option::take);
                if let Some(node) = node {
                    self.pending_refreshes.borrow_mut().remove(id);
                    removed.push(node);
                }
            }
        }
        if removed.is_empty() && schemas.is_empty() {
            return RetiredGraphDeclarations::default();
        }
        self.named.borrow_mut().retain(|_, cell| {
            !removed.iter().any(|node| {
                self.nodes
                    .borrow()
                    .get(cell.node)
                    .is_none_or(Option::is_none)
                    && node.port.readable().same_source(&cell.readable)
            })
        });
        self.topology.set(self.topology.get() + 1);
        RetiredGraphDeclarations {
            nodes: removed,
            schemas,
        }
    }
    pub(super) fn flush_retirements(self: &Rc<Self>, retained: &[GraphDeclarationSet]) {
        if !self.ready.borrow().is_empty() {
            return;
        }
        let requests = std::mem::take(&mut *self.deferred_retirements.borrow_mut());
        for request in requests {
            let mut keep = request.retained;
            keep.extend_from_slice(retained);
            self.retire(&request.declarations, &keep).finish();
        }
    }
}
