mod inbound;
mod outbound;

use super::*;
use crate::rpc::reload::{Operation, RemoteFacet, Summary, Timing, Vote};
use crate::{FacetKind, host::reload::PreparedReload};
use std::collections::VecDeque;

type VoteReply = oneshot::Sender<Result<Summary>>;
type AckReply = oneshot::Sender<Result<()>>;
struct Outbound {
    transaction: PositiveId,
    epoch: PositiveId,
    ready: bool,
    committed: bool,
    vote: Option<VoteReply>,
    acknowledge: Option<AckReply>,
    observer: Option<oneshot::Receiver<Result<()>>>,
}
struct Inbound {
    transaction: PositiveId,
    epoch: PositiveId,
    operation: Operation,
    prepared: RefCell<Option<PreparedReload>>,
}
#[derive(Default)]
pub(super) struct PeerState {
    epoch: Cell<u64>,
    synced: Cell<bool>,
    received: Cell<u64>,
    inbound: RefCell<Option<Rc<Inbound>>>,
    outbound: RefCell<Option<Outbound>>,
    ended: Cell<Option<(u64, u64, bool)>>,
    outbound_ended: RefCell<VecDeque<(u64, u64, bool)>>,
}

impl Connection {
    fn reload_host(&self) -> Result<Rc<HostInner>> {
        self.inner
            .host
            .upgrade()
            .ok_or_else(|| RuntimeError::unavailable("closed", "reload Host ended"))
    }
    pub(crate) fn reload_peer_name(&self) -> &str {
        match self.profile() {
            ConnectionProfile::AcceptedChild { expected }
            | ConnectionProfile::TrustedParent { expected } => expected,
            _ => "client",
        }
    }
    pub(crate) fn reload_synced(&self) -> bool {
        self.status() == ConnectionStatus::Open && self.inner.reload.synced.get()
    }
    pub(crate) fn reload_peer_epoch(&self) -> u64 {
        self.inner.reload.epoch.get()
    }
    pub(super) fn reload_hello(&self, epoch: Option<u64>) -> Result<bool> {
        let Some(epoch) = epoch else {
            self.inner.reload.synced.set(true);
            return Ok(true);
        };
        self.inner.reload.epoch.set(epoch);
        let host = self.reload_host()?;
        let current = host.reload_current_version();
        match self.profile() {
            ConnectionProfile::AcceptedChild { .. } if epoch > current => Err(RuntimeError::new(
                crate::ErrorCode::UpgradeRequired,
                "reload_epoch",
                "child is ahead of its parent",
            )),
            ConnectionProfile::AcceptedChild { .. }
                if epoch < current || host.reload_administration().is_coordinating() =>
            {
                host.reconcile_reload_child(self.clone());
                Ok(false)
            }
            _ => {
                self.inner.reload.synced.set(true);
                Ok(true)
            }
        }
    }
    pub(crate) fn reload_mark_synced(&self, epoch: PositiveId) {
        self.inner.reload.epoch.set(epoch.get());
        self.inner.reload.synced.set(true);
        self.inner.opened.cancel(RuntimeError::unavailable(
            "open",
            "reload peer is synchronized",
        ));
    }
    pub(crate) fn reload_flush(&self) {
        self.flush();
    }
    pub(crate) fn reload_fail(&self, error: RuntimeError) {
        self.fail(error);
    }
    pub(crate) fn reload_stopping(&self, error: RuntimeError) {
        let inbound = self.inner.reload.inbound.borrow().clone();
        if let Some(inbound) = inbound {
            self.stop_inbound(inbound, error);
        }
    }
    pub(super) fn reload_closed(&self, error: RuntimeError) {
        let inbound = self.inner.reload.inbound.borrow().clone();
        let outbound = self.inner.reload.outbound.borrow_mut().take();
        drop(outbound);
        if let Some(inbound) = inbound {
            self.stop_inbound(inbound, error);
        }
    }
}
fn clear_inbound(weak: &Weak<ConnectionInner>, expected: &Rc<Inbound>) {
    if let Some(inner) = weak.upgrade() {
        let removed = {
            let mut inbound = inner.reload.inbound.borrow_mut();
            if inbound
                .as_ref()
                .is_some_and(|state| Rc::ptr_eq(state, expected))
            {
                inbound.take()
            } else {
                None
            }
        };
        drop(removed);
    }
}
fn phase_error() -> RuntimeError {
    RuntimeError::invalid("reload_phase", "unexpected reload transaction phase")
}
fn loader_panic() -> RuntimeError {
    RuntimeError::invalid("reload_loader_panic", "participant loader panicked")
}

impl PeerState {
    fn ended_outbound(&self, transaction: PositiveId, epoch: PositiveId, committed: bool) -> bool {
        self.outbound_ended
            .borrow()
            .contains(&(transaction.get(), epoch.get(), committed))
    }
    fn remember_outbound(&self, state: &Outbound) {
        self.outbound_ended.borrow_mut().push_back((
            state.transaction.get(),
            state.epoch.get(),
            state.committed,
        ));
    }
    fn acknowledged_outbound(&self, transaction: PositiveId) {
        // A later response on the same FIFO proves earlier aborts were processed.
        self.outbound_ended
            .borrow_mut()
            .retain(|(ended, _, _)| *ended >= transaction.get());
    }
}
