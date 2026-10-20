use super::super::StateEvent;
use super::*;
use futures::{
    FutureExt,
    future::{LocalBoxFuture, try_join_all},
};

impl StateGraph {
    pub fn initialize(self: &Rc<Self>, values: Vec<(Cell, Value)>) -> Result<()> {
        self.check()?;
        (self.read_guard)()?;
        let _guard = Preparing::new(self)?;
        let mut candidates = BTreeMap::new();
        for (cell, value) in values {
            if !Weak::ptr_eq(&cell.graph, &Rc::downgrade(self)) {
                return Err(RuntimeError::invalid(
                    "foreign_state",
                    "initializer cell belongs to another graph",
                ));
            }
            if self.node(cell.node)?.port.current().is_ok() || candidates.contains_key(&cell.node) {
                return Err(RuntimeError::invalid(
                    "state_initialized",
                    "cell already has an initial value",
                ));
            }
            let value = normalize(&value)?;
            Self::validate(self.node(cell.node)?.as_ref(), &value)?;
            candidates.insert(cell.node, value);
        }
        self.derive_candidates(&mut candidates, &mut BTreeMap::new(), true, false)?;
        let candidates = candidates
            .into_iter()
            .map(|(node, value)| Candidate { node, value })
            .collect::<Vec<_>>();
        let mut waiters = Vec::new();
        let mut insertions = Vec::new();
        for candidate in &candidates {
            let node = self.node(candidate.node)?;
            if node.port.is_terminated() {
                return Err(gone());
            }
            insertions.push((
                node,
                Snapshot::normalized(candidate.value.clone(), self.epoch.get()),
            ));
        }
        // Initializer insertion makes storage readable, without publishing observer events.
        // Every fallible check precedes the insertion of the validated subset.
        for (node, snapshot) in insertions {
            node.unpublished.set(true);
            node.generation.set(node.generation.get() + 1);
            waiters.extend(node.port.initialize_silent(snapshot));
        }
        drop(_guard);
        for waiter in waiters {
            waiter.wake();
        }
        Ok(())
    }

    pub(crate) fn prepare_activation(&self) -> Result<StateActivation<'_>> {
        self.check()?;
        (self.read_guard)()?;
        let _guard = Preparing::new(self)?;
        let mut candidates = BTreeMap::new();
        for (id, node) in self.nodes.borrow().iter().enumerate() {
            let Some(node) = node else {
                continue;
            };
            if !node.installed.get() {
                continue;
            }
            if !matches!(node.kind, Kind::Derived { .. }) {
                let value = node.port.current()?.shared_value();
                Self::validate(node, &value)?;
                candidates.insert(id, value);
            }
        }
        self.derive_candidates(&mut candidates, &mut BTreeMap::new(), false, true)?;
        let candidates = candidates
            .into_iter()
            .map(|(node, value)| Candidate { node, value })
            .collect::<Vec<_>>();
        let prepared = self.prepare_commit(&candidates, None, false)?;
        Ok(StateActivation {
            graph: self,
            guard: Some(_guard),
            prepared: Some(prepared),
            topology: self.topology.get(),
            epoch: self.epoch.get(),
        })
    }

    #[cfg(test)]
    pub fn activate(self: &Rc<Self>) -> Result<()> {
        let mut activation = self.prepare_activation()?;
        activation.preflight()?;
        let hold = self.queue.hold();
        self.queue.append_batch(activation.adopt());
        activation.finish();
        drop(hold);
        Ok(())
    }

    pub fn hydrate_external(self: &Rc<Self>) -> LocalBoxFuture<'static, Result<()>> {
        let weak = Rc::downgrade(self);
        let stopped = self.stopped.clone();
        async move {
            let imports: Vec<_> = {
                let graph = weak.upgrade().ok_or_else(gone)?;
                graph.check()?;
                graph
                    .nodes
                    .borrow()
                    .iter()
                    .enumerate()
                    .filter_map(|(id, node)| {
                        let node = node.as_ref()?;
                        if !node.installed.get() {
                            return None;
                        }
                        if let Kind::Import {
                            source,
                            subscription,
                        } = &node.kind
                        {
                            Some((id, source.clone(), subscription.borrow().is_none()))
                        } else {
                            None
                        }
                    })
                    .collect()
            };
            for (id, source, attach) in &imports {
                if !attach {
                    continue;
                }
                let target = weak.clone();
                let node_id = *id;
                let subscription = source.subscribe_events(move |event| {
                    if let Some(graph) = target.upgrade()
                        && let Err(error) = graph.refresh(node_id, event)
                    {
                        (graph.reporter)(error);
                    }
                    Ok(())
                })?;
                let graph = weak.upgrade().ok_or_else(gone)?;
                let node = graph.node(*id)?;
                if let Kind::Import {
                    subscription: slot, ..
                } = &node.kind
                {
                    *slot.borrow_mut() = Some(subscription);
                }
            }
            let hydrated =
                try_join_all(imports.iter().map(|(_, source, _)| source.await_hydrated()))
                    .boxed_local();
            match futures::future::select(hydrated, stopped.cancelled()).await {
                futures::future::Either::Left((snapshots, _)) => snapshots?,
                futures::future::Either::Right((error, _)) => return Err(error),
            };
            let graph = weak.upgrade().ok_or_else(gone)?;
            for (id, source, _) in imports {
                graph.refresh(id, StateEvent::Snapshot(source.snapshot()?))?;
            }
            Ok(())
        }
        .boxed_local()
    }

    pub(super) fn refresh(&self, id: NodeId, event: StateEvent) -> Result<()> {
        if let Some(error) = &*self.ended.borrow() {
            return Err(error.clone());
        }
        if self.preparing.get() || self.ready.borrow().contains_key(&id) {
            let mut pending = self.pending_refreshes.borrow_mut();
            let reset = pending.entry(id).or_default();
            *reset |= matches!(event, StateEvent::Reset(_));
            return Ok(());
        }
        (self.read_guard)()?;
        let _guard = Preparing::new(self)?;
        let value = normalize(event.snapshot().value())?;
        Self::validate(self.node(id)?.as_ref(), &value)?;
        let mut candidates = BTreeMap::from([(id, value)]);
        self.derive_candidates(
            &mut candidates,
            &mut BTreeMap::new(),
            !self.active.get(),
            false,
        )?;
        let candidates = candidates
            .into_iter()
            .map(|(node, value)| Candidate { node, value })
            .collect::<Vec<_>>();
        let hold = self.queue.hold();
        let result = self.commit(&candidates, None, matches!(event, StateEvent::Reset(_)));
        drop(_guard);
        drop(hold);
        result
    }

    pub(super) fn flush_refreshes(&self) {
        if self.preparing.get() || std::thread::panicking() || self.refreshing.replace(true) {
            return;
        }
        struct Refreshing<'a>(&'a Flag<bool>);
        impl Drop for Refreshing<'_> {
            fn drop(&mut self) {
                self.0.set(false);
            }
        }
        let _refreshing = Refreshing(&self.refreshing);
        loop {
            let all = std::mem::take(&mut *self.pending_refreshes.borrow_mut());
            let (blocked, pending): (BTreeMap<_, _>, BTreeMap<_, _>) = all
                .into_iter()
                .partition(|(id, _)| self.ready.borrow().contains_key(id));
            self.pending_refreshes.borrow_mut().extend(blocked);
            if pending.is_empty() {
                break;
            }
            for (id, reset) in pending {
                let result = (|| {
                    let node = self.node(id)?;
                    let Kind::Import { source, .. } = &node.kind else {
                        return Err(gone());
                    };
                    let snapshot = source.snapshot()?;
                    let event = if reset {
                        StateEvent::Reset(snapshot)
                    } else {
                        StateEvent::Snapshot(snapshot)
                    };
                    self.refresh(id, event)
                })();
                if let Err(error) = result {
                    (self.reporter)(error);
                }
            }
        }
    }

    pub fn terminate(&self, error: RuntimeError) {
        if self.ended.borrow().is_some() {
            return;
        }
        *self.ended.borrow_mut() = Some(error.clone());
        self.pending_refreshes.borrow_mut().clear();
        self.stopped.cancel(error.clone());
        let nodes = self.nodes.borrow().clone();
        let subscriptions: Vec<_> = nodes
            .iter()
            .filter_map(|node| {
                let node = node.as_ref()?;
                if let Kind::Import { subscription, .. } = &node.kind {
                    subscription.borrow_mut().take()
                } else {
                    None
                }
            })
            .collect();
        drop(subscriptions);
        for node in nodes.into_iter().flatten() {
            node.port.terminate(error.clone());
        }
        let initializers = std::mem::take(&mut *self.initializers.borrow_mut());
        let nodes = std::mem::take(&mut *self.nodes.borrow_mut());
        drop(initializers);
        drop(nodes);
    }
}

/// A frozen initial graph candidate. No observer runs before explicit adoption and finish.
pub(crate) struct StateActivation<'a> {
    graph: &'a StateGraph,
    guard: Option<Preparing<'a>>,
    prepared: Option<super::mutation::PreparedCommit>,
    topology: u64,
    epoch: u64,
}
impl StateActivation<'_> {
    pub(crate) fn preflight(&self) -> Result<()> {
        if let Some(error) = self.graph.ended.borrow().clone() {
            return Err(error);
        }
        if self.graph.topology.get() != self.topology || self.graph.epoch.get() != self.epoch {
            return Err(RuntimeError::invalid(
                "stale_state",
                "initial graph changed during preparation",
            ));
        }
        if self.prepared.as_ref().is_some_and(|prepared| {
            prepared
                .events
                .iter()
                .any(|(node, _)| node.port.is_terminated())
        }) {
            return Err(gone());
        }
        Ok(())
    }
    pub(crate) fn adopt(&mut self) -> Vec<super::super::publication::PendingPublication> {
        let prepared = self.prepared.take().expect("initial state adopted once");
        let batch = self.graph.adopt_commit(prepared);
        self.graph.active.set(true);
        batch
    }
    pub(crate) fn finish(mut self) {
        drop(self.guard.take());
    }
}
