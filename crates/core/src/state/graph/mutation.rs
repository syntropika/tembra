use super::super::{
    StateEvent,
    cell::{Prepared, PreparedStatus},
};
use super::*;
use crate::{
    Context,
    delta::{DeltaBatch, DeltaOp, diff},
};

impl StateGraph {
    pub(super) fn stamp(node: NodeId, value: &Node) -> Result<ReadStamp> {
        Ok(ReadStamp {
            node,
            revision: match value.port.current() {
                Ok(snapshot) => Some(snapshot.revision()),
                Err(error) if error.code == ErrorCode::NotReady => None,
                Err(error) => return Err(error),
            },
            generation: value.generation.get(),
        })
    }

    pub(super) fn derive_candidates(
        &self,
        candidates: &mut BTreeMap<NodeId, Rc<Value>>,
        reads: &mut BTreeMap<NodeId, ReadStamp>,
        skip_pending: bool,
        all: bool,
    ) -> Result<()> {
        let nodes = self.nodes.borrow().clone();
        for (id, node) in nodes.iter().enumerate() {
            let Some(node) = node else {
                continue;
            };
            if !node.installed.get() {
                continue;
            }
            let Kind::Derived { sources, compute } = &node.kind else {
                continue;
            };
            if !all && !sources.iter().any(|source| candidates.contains_key(source)) {
                continue;
            }
            let mut snapshots = Vec::new();
            let mut missing = false;
            for source in sources {
                let source_node = nodes[*source].as_ref().ok_or_else(gone)?;
                let snapshot = source_node.port.current();
                reads
                    .entry(*source)
                    .or_insert(Self::stamp(*source, source_node)?);
                if let Some(value) = candidates.get(source) {
                    snapshots.push(Snapshot::normalized(value.clone(), self.epoch.get()));
                } else {
                    match snapshot {
                        Ok(snapshot) => snapshots.push(snapshot),
                        Err(error) if skip_pending && error.code == ErrorCode::NotReady => {
                            missing = true;
                        }
                        Err(error) => return Err(error),
                    }
                }
            }
            if missing {
                continue;
            }
            let value = normalize(&compute(&snapshots)?)?;
            Self::validate(node, &value)?;
            candidates.insert(id, value);
        }
        Ok(())
    }

    pub(in crate::state) fn prepare(
        self: &Rc<Self>,
        cell: &Cell,
        ctx: &Context,
        root_diff: bool,
        change: impl FnOnce(&Value) -> Result<Value>,
    ) -> Result<Prepared> {
        self.check()?;
        (self.write_guard)(ctx)?;
        self.check_ready(cell.node)?;
        if !Weak::ptr_eq(&cell.graph, &Rc::downgrade(self)) {
            return Err(RuntimeError::invalid(
                "foreign_state",
                "cell belongs to another graph",
            ));
        }
        let node = self.node(cell.node)?;
        let current = node.port.current()?;
        let topology = self.topology.get();
        let _guard = Preparing::new(self)?;
        let candidate = normalize(&change(current.value())?)?;
        Self::validate(&node, &candidate)?;
        let mut reads = BTreeMap::from([(cell.node, Self::stamp(cell.node, &node)?)]);
        let mut candidates = BTreeMap::new();
        if candidate.as_ref() != current.value() {
            candidates.insert(cell.node, candidate);
            self.derive_candidates(&mut candidates, &mut reads, !self.active.get(), false)?;
        }
        Ok(Prepared {
            graph: Rc::downgrade(self),
            node: cell.node,
            context: ctx.clone(),
            topology,
            reads: reads.into_values().collect(),
            candidates: candidates
                .into_iter()
                .map(|(node, value)| Candidate { node, value })
                .collect(),
            root_diff,
            status: Flag::new(PreparedStatus::Pending),
        })
    }

    pub(in crate::state) fn adopt(
        self: &Rc<Self>,
        cell: &Cell,
        ctx: &Context,
        prepared: &Prepared,
    ) -> Result<Snapshot> {
        self.check()?;
        (self.write_guard)(ctx)?;
        self.check_ready(cell.node)?;
        if !Weak::ptr_eq(&prepared.graph, &Rc::downgrade(self)) || prepared.node != cell.node {
            return Err(RuntimeError::invalid(
                "foreign_prepared",
                "candidate belongs to another cell",
            ));
        }
        if !ctx.same_scope(&prepared.context) {
            return Err(RuntimeError::invalid(
                "foreign_prepared",
                "candidate belongs to another context scope",
            ));
        }
        (self.write_guard)(&prepared.context)?;
        if prepared.status.get() != PreparedStatus::Pending {
            return Err(RuntimeError::invalid(
                "prepared_state",
                "candidate was aborted or consumed",
            ));
        }
        if prepared.topology != self.topology.get() {
            return Err(RuntimeError::invalid(
                "stale_prepared",
                "state topology or exposures changed",
            ));
        }
        for read in &prepared.reads {
            let node = self.node(read.node)?;
            let current = Self::stamp(read.node, &node)?;
            if current.generation != read.generation || current.revision != read.revision {
                return Err(RuntimeError::invalid(
                    "stale_prepared",
                    "a candidate dependency changed",
                ));
            }
        }
        let hold = self.queue.hold();
        self.commit(
            &prepared.candidates,
            Some((cell.node, prepared.root_diff)),
            false,
        )?;
        self.node(cell.node)?
            .generation
            .set(self.node(cell.node)?.generation.get() + 1);
        prepared.status.set(PreparedStatus::Consumed);
        let result = self.node(cell.node)?.port.current();
        drop(hold);
        result
    }

    pub(super) fn prepare_commit(
        &self,
        candidates: &[Candidate],
        root: Option<(NodeId, bool)>,
        reset: bool,
    ) -> Result<PreparedCommit> {
        let next = self
            .epoch
            .get()
            .checked_add(1)
            .filter(|next| *next <= MAX_REVISION)
            .ok_or_else(|| {
                RuntimeError::unavailable("state_revision", "state revision space exhausted")
            })?;
        let mut events = Vec::new();
        for candidate in candidates {
            let node = self.node(candidate.node)?;
            if node.port.is_terminated() {
                return Err(gone());
            }
            Self::validate(&node, &candidate.value)?;
            let snapshot = Snapshot::normalized(candidate.value.clone(), next);
            let event = match node.port.current() {
                Ok(previous)
                    if previous.value() == candidate.value.as_ref()
                        && !reset
                        && !node.unpublished.get() =>
                {
                    continue;
                }
                Ok(previous)
                    if previous.value() == candidate.value.as_ref() && node.unpublished.get() =>
                {
                    StateEvent::Snapshot(previous)
                }
                Ok(_) if reset => StateEvent::Reset(snapshot),
                Ok(previous) => {
                    let structural =
                        root.is_none_or(|(id, structural)| id != candidate.node || structural);
                    let ops = if structural {
                        diff(previous.value(), &candidate.value)
                            .map_err(|error| RuntimeError::invalid("delta", error.to_string()))?
                    } else {
                        vec![DeltaOp::Set {
                            path: Vec::new(),
                            value: candidate.value.as_ref().clone(),
                        }]
                    };
                    let delta = DeltaBatch::new(previous.revision(), next, ops)
                        .map_err(|error| RuntimeError::invalid("delta", error.to_string()))?;
                    StateEvent::Ops { delta, snapshot }
                }
                Err(error) if error.code == ErrorCode::NotReady => StateEvent::Snapshot(snapshot),
                Err(error) => return Err(error),
            };
            events.push((node, event));
        }
        Ok(PreparedCommit { next, events })
    }

    pub(super) fn adopt_commit(
        &self,
        prepared: PreparedCommit,
    ) -> Vec<super::super::publication::PendingPublication> {
        if prepared.events.is_empty() {
            return Vec::new();
        }
        self.epoch.set(prepared.next);
        prepared
            .events
            .into_iter()
            .map(|(node, event)| {
                node.generation.set(node.generation.get() + 1);
                node.unpublished.set(false);
                node.port
                    .adopt(event)
                    .expect("owned port was checked live before synchronous adoption")
            })
            .collect()
    }

    pub(super) fn commit(
        &self,
        candidates: &[Candidate],
        root: Option<(NodeId, bool)>,
        reset: bool,
    ) -> Result<()> {
        let prepared = self.prepare_commit(candidates, root, reset)?;
        let batch = self.adopt_commit(prepared);
        self.queue.append_batch(batch);
        Ok(())
    }
}

pub(super) struct PreparedCommit {
    pub next: u64,
    pub events: Vec<(Rc<Node>, StateEvent)>,
}
