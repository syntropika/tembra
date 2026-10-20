//! One-level reload administration over the existing role-checked FIFO connections.
mod coordinator;
mod types;
pub(crate) use coordinator::reconcile_child;
pub(crate) use types::validate_batch as validate_remote_batch;
pub use types::{ClusterReloadReport, ParticipantReloadReport, ReloadFacetLoader, RemoteFacet};
pub(crate) use types::{Summary, Timing, Vote};

use crate::{CancellationToken, Result, RuntimeError};
use futures::{FutureExt, future::LocalBoxFuture};
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    rc::{Rc, Weak},
};

#[derive(Default)]
pub(crate) struct Administration {
    loader: RefCell<Option<ReloadFacetLoader>>,
    desired: RefCell<BTreeMap<String, RemoteFacet>>,
    removed: RefCell<BTreeSet<String>>,
    sequence: Cell<u64>,
    busy: Cell<bool>,
    changed: RefCell<CancellationToken>,
    stopped: CancellationToken,
    active: RefCell<Vec<Weak<OperationControl>>>,
}
pub(crate) struct OperationControl {
    pub(crate) cancellation: CancellationToken,
    settled: CancellationToken,
}
pub(crate) struct Operation {
    control: Rc<OperationControl>,
}
impl Operation {
    pub(crate) fn cancellation(&self) -> CancellationToken {
        self.control.cancellation.clone()
    }
}
impl Drop for Operation {
    fn drop(&mut self) {
        self.control.settled.cancel(RuntimeError::aborted(
            "settled",
            "reload administration physically settled",
        ));
    }
}
struct CoordinatorSlot(Rc<Administration>);
impl Drop for CoordinatorSlot {
    fn drop(&mut self) {
        self.0.busy.set(false);
        let changed = self.0.changed.replace(CancellationToken::new());
        changed.cancel("reload administration changed");
    }
}
impl Administration {
    pub(crate) fn is_coordinating(&self) -> bool {
        self.busy.get()
    }
    pub(crate) fn begin(&self, parent: Option<&CancellationToken>) -> Result<Operation> {
        self.stopped.check()?;
        if let Some(parent) = parent {
            parent.check()?;
        }
        let control = Rc::new(OperationControl {
            cancellation: parent.map(CancellationToken::child).unwrap_or_default(),
            settled: CancellationToken::new(),
        });
        let mut active = self.active.borrow_mut();
        active.retain(|entry| entry.strong_count() > 0);
        if active.len() >= 128 {
            return Err(RuntimeError::unavailable(
                "reload_operations",
                "physical reload administration capacity exhausted",
            ));
        }
        active.push(Rc::downgrade(&control));
        Ok(Operation { control })
    }
    fn slot(self: &Rc<Self>) -> Result<CoordinatorSlot> {
        self.stopped.check()?;
        if self.busy.replace(true) {
            return Err(RuntimeError::unavailable(
                "reload_busy",
                "a cluster reload or child reconciliation is already active",
            ));
        }
        Ok(CoordinatorSlot(self.clone()))
    }
    pub(crate) fn loader(&self) -> Result<ReloadFacetLoader> {
        self.loader.borrow().clone().ok_or_else(|| {
            RuntimeError::unavailable("reload_loader", "no participant facet loader is configured")
        })
    }
    pub(crate) fn stop(&self, cause: RuntimeError) {
        let active = self
            .active
            .borrow()
            .iter()
            .filter_map(Weak::upgrade)
            .collect::<Vec<_>>();
        self.stopped.cancel(cause.clone());
        for entry in active {
            entry.cancellation.cancel(cause.clone());
        }
    }
    pub(crate) fn settled(&self) -> LocalBoxFuture<'static, ()> {
        let active = self
            .active
            .borrow()
            .iter()
            .filter_map(Weak::upgrade)
            .collect::<Vec<_>>();
        async move {
            for entry in active {
                entry.settled.cancelled().await;
            }
        }
        .boxed_local()
    }
    fn next_transaction(&self) -> Result<super::protocol::PositiveId> {
        let next = self.sequence.get().checked_add(1).ok_or_else(|| {
            RuntimeError::unavailable("reload_id", "reload transaction identities exhausted")
        })?;
        let checked = super::protocol::PositiveId::new(next)?;
        self.sequence.set(next);
        Ok(checked)
    }
    fn remember(&self, facets: &[RemoteFacet], remove: &[String]) {
        let mut desired = self.desired.borrow_mut();
        let mut removed = self.removed.borrow_mut();
        for name in remove {
            desired.remove(name);
            removed.insert(name.clone());
        }
        for facet in facets {
            removed.remove(facet.name());
            desired.insert(facet.name().into(), facet.clone());
        }
    }
    fn check_remembered(&self, facets: &[RemoteFacet], remove: &[String]) -> Result<()> {
        let mut desired = self.desired.borrow().clone();
        let mut removed = self.removed.borrow().clone();
        for name in remove {
            desired.remove(name);
            removed.insert(name.clone());
        }
        for facet in facets {
            removed.remove(facet.name());
            desired.insert(facet.name().to_owned(), facet.clone());
        }
        let bytes = desired
            .values()
            .map(|facet| facet.name().len() + facet.artifact().len())
            .sum::<usize>()
            + removed.iter().map(String::len).sum::<usize>();
        if desired.len() > 128 || removed.len() > 128 || bytes > 65_536 {
            return Err(RuntimeError::unavailable(
                "reload_history",
                "remembered deployment metadata capacity exhausted",
            ));
        }
        Ok(())
    }
}
