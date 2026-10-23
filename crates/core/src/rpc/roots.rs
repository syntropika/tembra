use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    rc::{Rc, Weak},
};

use serde_json::Value;

use super::protocol::{ConnectionId, PositiveId, RootId, RootOrigin, RootRef};
use crate::{
    CancellationToken, Context, ErrorCode, Result, RuntimeError, ScopeGuard,
    host::{HostAttachment, HostInner},
};

struct RootState {
    local: u64,
    version: u64,
    touched: RefCell<HashSet<u64>>,
    wire: RefCell<Option<RootRef>>,
    foreign: bool,
    owner_live: Cell<bool>,
    stopped: CancellationToken,
    carriers: RefCell<HashSet<ConnectionId>>,
    exports: RefCell<HashSet<ConnectionId>>,
    entries: RefCell<HashMap<u64, (ConnectionId, CancellationToken)>>,
}

struct RootLease {
    registry: Weak<RootRegistry>,
    state: Rc<RootState>,
}

pub(crate) struct RootRegistry {
    host: Weak<HostInner>,
    identity: RefCell<Option<RootOrigin>>,
    sequence: Cell<u64>,
    entry_sequence: Cell<u64>,
    records: RefCell<HashMap<RootId, Rc<RootState>>>,
    local: RefCell<HashMap<u64, Weak<RootState>>>,
    changed: RefCell<CancellationToken>,
    ended: RefCell<HashSet<(ConnectionId, RootId)>>,
    incoming: Cell<usize>,
}

impl RootRegistry {
    pub(crate) fn new(host: Weak<HostInner>) -> Rc<Self> {
        Rc::new(Self {
            host,
            identity: RefCell::new(None),
            sequence: Cell::new(0),
            entry_sequence: Cell::new(0),
            records: RefCell::new(HashMap::new()),
            local: RefCell::new(HashMap::new()),
            changed: RefCell::new(CancellationToken::new()),
            ended: RefCell::new(HashSet::new()),
            incoming: Cell::new(0),
        })
    }

    pub(crate) fn notify_change(&self) {
        let previous = self.changed.replace(CancellationToken::new());
        previous.cancel("root or physical entry changed");
    }

    pub(crate) fn epoch_idle(&self, version: u64) -> bool {
        !self
            .local
            .borrow()
            .values()
            .filter_map(Weak::upgrade)
            .any(|state| state.version == version && root_alive(&state))
    }
    pub(crate) fn changed(&self) -> CancellationToken {
        self.changed.borrow().clone()
    }

    pub(crate) fn touch(
        &self,
        context: &Context,
        object: &Rc<crate::object::ObjectRecord>,
    ) -> Result<()> {
        if context.code_version() < object.min_code_version.get() {
            return Err(RuntimeError::new(
                ErrorCode::Superseded,
                "upgrade_conflict",
                "object state requires newer code",
            ));
        }
        if let Some(allowed) = object.gate.borrow().as_ref()
            && !allowed.contains(&context.root_id())
        {
            return Err(RuntimeError::new(
                ErrorCode::UpgradeConflict,
                "upgrade_conflict",
                "root reached a gated object too late",
            ));
        }
        let lease = context
            .attachment::<RootLease>()
            .ok_or_else(|| RuntimeError::invalid("root", "context has no issued root lease"))?;
        lease.state.touched.borrow_mut().insert(object.id);
        Ok(())
    }

    pub(crate) fn begin_gate(
        self: &Rc<Self>,
        objects: &[Rc<crate::object::ObjectRecord>],
    ) -> Result<RootGate> {
        let states: Vec<_> = self
            .local
            .borrow()
            .values()
            .filter_map(Weak::upgrade)
            .collect();
        let mut gated = Vec::new();
        for object in objects {
            if object.gate.borrow().is_some() {
                for previous in &gated {
                    let previous: &Weak<crate::object::ObjectRecord> = previous;
                    if let Some(previous) = previous.upgrade() {
                        previous.gate.borrow_mut().take();
                    }
                }
                return Err(RuntimeError::unavailable(
                    "reload_busy",
                    "object already gated",
                ));
            }
            let allowed = states
                .iter()
                .filter(|state| {
                    state.touched.borrow().contains(&object.id)
                        && root_alive(state)
                        && self
                            .host
                            .upgrade()
                            .is_some_and(|host| host.epochs.exact(state.version).is_ok())
                })
                .map(|state| state.local)
                .collect();
            *object.gate.borrow_mut() = Some(allowed);
            gated.push(Rc::downgrade(object));
        }
        Ok(RootGate {
            registry: Rc::downgrade(self),
            objects: gated,
            active: true,
        })
    }

    pub(crate) fn set_identity(&self, identity: RootOrigin) -> Result<()> {
        let host = self.host.upgrade().ok_or_else(closed)?;
        if identity.host() != host.name {
            return Err(RuntimeError::invalid(
                "rpc_identity",
                "RPC identity must name this Host",
            ));
        }
        let mut current = self.identity.borrow_mut();
        if current.as_ref().is_some_and(|current| current != &identity) {
            return Err(RuntimeError::invalid(
                "rpc_identity",
                "RPC identity is already installed",
            ));
        }
        *current = Some(identity);
        Ok(())
    }

    pub(crate) fn identity(&self) -> Result<RootOrigin> {
        self.identity.borrow().clone().ok_or_else(|| {
            RuntimeError::invalid(
                "rpc_identity",
                "install an RPC incarnation before connecting trusted hosts",
            )
        })
    }

    pub(crate) fn register_origin(self: &Rc<Self>, context: &Context) {
        let state = Rc::new(RootState {
            local: context.root_id(),
            version: context.code_version(),
            touched: RefCell::new(HashSet::new()),
            wire: RefCell::new(None),
            foreign: false,
            owner_live: Cell::new(true),
            stopped: context.cancellation().child(),
            carriers: RefCell::new(HashSet::new()),
            exports: RefCell::new(HashSet::new()),
            entries: RefCell::new(HashMap::new()),
        });
        self.local
            .borrow_mut()
            .insert(state.local, Rc::downgrade(&state));
        context.set_attachment(Rc::new(RootLease {
            registry: Rc::downgrade(self),
            state: state.clone(),
        }));
        let registry = Rc::downgrade(self);
        // Only HostInner::new_root invokes this hook, before exposing the fresh context.
        let installed = context.on_settle(move || {
            state.owner_live.set(false);
            state.stopped.cancel(RuntimeError::aborted(
                "expired",
                "origin root scope settled",
            ));
            if let Some(registry) = registry.upgrade() {
                registry.send_end(&state);
                registry.retire(&state);
                registry.notify_change();
            }
        });
        debug_assert!(
            installed.is_ok(),
            "origin registration requires a fresh owning scope"
        );
    }

    pub(crate) fn export(&self, context: &Context, link: ConnectionId) -> Result<RootRef> {
        let host = self.host.upgrade().ok_or_else(closed)?;
        context.check(host.id)?;
        let connection = host.rpc_connection(link).ok_or_else(closed)?;
        if !connection.trusted() {
            return Err(RuntimeError::new(
                ErrorCode::Forbidden,
                "root",
                "clients cannot export execution roots",
            ));
        }
        let lease = context
            .attachment::<RootLease>()
            .ok_or_else(|| RuntimeError::invalid("root", "context has no issued root lease"))?;
        let registry = lease.registry.upgrade().ok_or_else(closed)?;
        if !std::ptr::eq(self, Rc::as_ptr(&registry)) {
            return Err(RuntimeError::invalid(
                "foreign_ctx",
                "root belongs to another Host",
            ));
        }
        if !lease.state.foreign && !lease.state.owner_live.get() {
            return Err(ended());
        }
        host.epochs.exact(context.code_version())?;
        if lease.state.version != context.code_version() {
            return Err(epoch());
        }
        let current = lease.state.wire.borrow().clone();
        let wire = if let Some(wire) = current {
            wire
        } else {
            let sequence = self.sequence.get().checked_add(1).ok_or_else(|| {
                RuntimeError::unavailable("root_limit", "root sequence exhausted")
            })?;
            let sequence = PositiveId::new(sequence)?;
            let identity = RootId {
                origin: self.identity()?,
                sequence,
            };
            let wire = RootRef {
                identity: identity.clone(),
                code_version: PositiveId::new(context.code_version())?,
            };
            self.sequence.set(sequence.get());
            *lease.state.wire.borrow_mut() = Some(wire.clone());
            self.records
                .borrow_mut()
                .insert(identity, lease.state.clone());
            wire
        };
        lease.state.exports.borrow_mut().insert(link);
        Ok(wire)
    }

    pub(crate) fn enter(
        self: &Rc<Self>,
        wire: Option<&RootRef>,
        link: ConnectionId,
        principal: Value,
    ) -> Result<RootEntry> {
        let host = self.host.upgrade().ok_or_else(closed)?;
        if self.incoming.get() >= host.rpc_limits.max_incoming_calls {
            return Err(RuntimeError::unavailable(
                "too_many_calls",
                "Host incoming call budget reached",
            ));
        }
        let (context, scope, state) = if let Some(wire) = wire {
            if !host
                .rpc_connection(link)
                .is_some_and(|connection| connection.trusted())
            {
                return Err(RuntimeError::new(
                    ErrorCode::Forbidden,
                    "root",
                    "untrusted root ingress",
                ));
            }
            host.epochs.exact(wire.code_version.get())?;
            if self.ended.borrow().contains(&(link, wire.identity.clone())) {
                return Err(ended());
            }
            let maximum = host
                .rpc_connection(link)
                .ok_or_else(closed)?
                .limits()
                .max_imported_roots;
            let needs_carrier = self
                .records
                .borrow()
                .get(&wire.identity)
                .is_none_or(|state| !state.carriers.borrow().contains(&link));
            if needs_carrier
                && wire.identity.origin != self.identity()?
                && self
                    .records
                    .borrow()
                    .values()
                    .filter(|state| state.foreign && state.carriers.borrow().contains(&link))
                    .count()
                    >= maximum
            {
                return Err(RuntimeError::unavailable(
                    "too_many_roots",
                    "connection imported root budget reached",
                ));
            }
            let current = self.records.borrow().get(&wire.identity).cloned();
            let own = wire.identity.origin == self.identity()?;
            let state = match current {
                Some(state) => {
                    if own && !state.owner_live.get() {
                        return Err(ended());
                    }
                    if state
                        .wire
                        .borrow()
                        .as_ref()
                        .is_some_and(|current| current.code_version != wire.code_version)
                    {
                        return Err(epoch());
                    }
                    state
                }
                None if own => {
                    return Err(RuntimeError::invalid(
                        "root",
                        "own-origin root was not issued or its owner ended",
                    ));
                }
                None => {
                    if self
                        .records
                        .borrow()
                        .values()
                        .filter(|state| state.foreign)
                        .count()
                        >= host.rpc_limits.max_imported_roots
                    {
                        return Err(RuntimeError::unavailable(
                            "too_many_roots",
                            "imported root limit reached",
                        ));
                    }
                    let state = Rc::new(RootState {
                        local: host.next_root_id(),
                        version: wire.code_version.get(),
                        touched: RefCell::new(HashSet::new()),
                        wire: RefCell::new(Some(wire.clone())),
                        foreign: true,
                        owner_live: Cell::new(false),
                        stopped: host
                            .epochs
                            .exact(wire.code_version.get())?
                            .cancellation
                            .child(),
                        carriers: RefCell::new(HashSet::new()),
                        exports: RefCell::new(HashSet::new()),
                        entries: RefCell::new(HashMap::new()),
                    });
                    self.records
                        .borrow_mut()
                        .insert(wire.identity.clone(), state.clone());
                    state
                }
            };
            if !state.carriers.borrow().contains(&link)
                && state.carriers.borrow().len()
                    >= host
                        .rpc_connection(link)
                        .ok_or_else(closed)?
                        .limits()
                        .max_root_carriers
            {
                return Err(RuntimeError::unavailable(
                    "too_many_carriers",
                    "root carrier limit reached",
                ));
            }
            state.carriers.borrow_mut().insert(link);
            let (context, scope) =
                Context::new_root(host.id, state.local, state.version, principal);
            let context = context
                .with_cancellation(state.stopped.child())
                .observe_root(host.observations.clone());
            context.set_attachment(Rc::new(HostAttachment(Rc::downgrade(&host))));
            context.set_attachment(Rc::new(RootLease {
                registry: Rc::downgrade(self),
                state: state.clone(),
            }));
            self.local
                .borrow_mut()
                .insert(state.local, Rc::downgrade(&state));
            (context, scope, state)
        } else {
            let (context, scope) = host.new_root(principal);
            let state = context
                .attachment::<RootLease>()
                .expect("Host issues root lease")
                .state
                .clone();
            (context, scope, state)
        };
        let entry =
            self.entry_sequence.get().checked_add(1).ok_or_else(|| {
                RuntimeError::unavailable("root_limit", "entry sequence exhausted")
            })?;
        self.entry_sequence.set(entry);
        state
            .entries
            .borrow_mut()
            .insert(entry, (link, context.cancellation().clone()));
        self.incoming.set(self.incoming.get() + 1);
        Ok(RootEntry {
            context,
            scope: Some(scope),
            state,
            registry: Rc::downgrade(self),
            entry,
        })
    }

    pub(crate) fn carrier_end(&self, link: ConnectionId, root: &RootId) {
        let state = self.records.borrow().get(root).cloned();
        let Some(state) = state else {
            return;
        };
        if !state.carriers.borrow().contains(&link) {
            return;
        }
        if state.foreign {
            let (per_connection, per_host) = self
                .host
                .upgrade()
                .and_then(|host| {
                    host.rpc_connection(link).map(|connection| {
                        (
                            connection.limits().max_ended_roots_per_connection,
                            host.rpc_limits
                                .max_ended_root_history
                                .min(connection.limits().max_ended_roots_per_host),
                        )
                    })
                })
                .unwrap_or((4096, 16_384));
            let limit = {
                let ended = self.ended.borrow();
                !ended.contains(&(link, root.clone()))
                    && (ended.len() >= per_host
                        || ended.iter().filter(|(id, _)| *id == link).count() >= per_connection)
            };
            if limit {
                if let Some(connection) = self
                    .host
                    .upgrade()
                    .and_then(|host| host.rpc_connection(link))
                {
                    connection.close(super::CloseReason::new(RuntimeError::unavailable(
                        "root_history",
                        "ended root history limit reached",
                    )));
                }
                return;
            }
            self.ended.borrow_mut().insert((link, root.clone()));
        }
        state.carriers.borrow_mut().remove(&link);
        self.retire(&state);
    }

    pub(crate) fn link_closed(&self, link: ConnectionId, reason: RuntimeError) {
        self.ended.borrow_mut().retain(|(id, _)| *id != link);
        let states: Vec<_> = self.records.borrow().values().cloned().collect();
        for state in states {
            state.carriers.borrow_mut().remove(&link);
            state.exports.borrow_mut().remove(&link);
            let tokens: Vec<_> = state
                .entries
                .borrow()
                .values()
                .filter(|(id, _)| *id == link)
                .map(|(_, token)| token.clone())
                .collect();
            for token in tokens {
                token.cancel(reason.clone());
            }
            self.retire(&state);
        }
    }

    pub(crate) fn close(&self, reason: RuntimeError) {
        let states: Vec<_> = self.records.borrow().values().cloned().collect();
        for state in states {
            state.stopped.cancel(reason.clone());
        }
    }

    fn send_end(&self, state: &RootState) {
        let links = std::mem::take(&mut *state.exports.borrow_mut());
        for link in links {
            if let Some(connection) = self
                .host
                .upgrade()
                .and_then(|host| host.rpc_connection(link))
            {
                let wire = state.wire.borrow().clone();
                if let Some(wire) = wire {
                    let _ = connection.root_end(&wire.identity);
                }
            }
        }
    }

    fn retire(&self, state: &Rc<RootState>) {
        self.notify_change();
        if !state.entries.borrow().is_empty() {
            return;
        }
        if (!state.foreign && state.owner_live.get())
            || (state.foreign && !state.carriers.borrow().is_empty())
        {
            return;
        }
        self.send_end(state);
        let wire = state.wire.borrow().clone();
        if let Some(wire) = wire {
            let retired = self.records.borrow_mut().remove(&wire.identity);
            drop(retired);
        }
        let retired = self.local.borrow_mut().remove(&state.local);
        drop(retired);
    }
}

pub(crate) struct RootEntry {
    context: Context,
    scope: Option<ScopeGuard>,
    state: Rc<RootState>,
    registry: Weak<RootRegistry>,
    entry: u64,
}
impl RootEntry {
    pub(crate) fn context(&self) -> &Context {
        &self.context
    }
}
impl Drop for RootEntry {
    fn drop(&mut self) {
        drop(self.scope.take());
        let entry = self.state.entries.borrow_mut().remove(&self.entry);
        drop(entry);
        if let Some(registry) = self.registry.upgrade() {
            registry
                .incoming
                .set(registry.incoming.get().saturating_sub(1));
            registry.retire(&self.state);
        }
    }
}
fn closed() -> RuntimeError {
    RuntimeError::unavailable("closed", "RPC Host or connection ended")
}
fn ended() -> RuntimeError {
    RuntimeError::aborted("root_ended", "root owner or carrier already ended")
}
fn epoch() -> RuntimeError {
    RuntimeError::new(
        ErrorCode::Superseded,
        "code_version",
        "root code version is not retained",
    )
}

fn root_alive(state: &RootState) -> bool {
    if state.foreign {
        !state.carriers.borrow().is_empty() || !state.entries.borrow().is_empty()
    } else {
        state.owner_live.get()
    }
}

/// Owns the temporary gate. Its drain includes physical users of context-free cell reads.
pub(crate) struct RootGate {
    registry: Weak<RootRegistry>,
    objects: Vec<Weak<crate::object::ObjectRecord>>,
    active: bool,
}
impl RootGate {
    pub(crate) fn drained(&self) -> futures::future::LocalBoxFuture<'static, Result<()>> {
        use futures::FutureExt;
        let (registry, objects) = (self.registry.clone(), self.objects.clone());
        async move {
            loop {
                let registry = registry.upgrade().ok_or_else(closed)?;
                let wait = registry.changed.borrow().clone();
                let states: Vec<_> = registry
                    .local
                    .borrow()
                    .values()
                    .filter_map(Weak::upgrade)
                    .collect();
                let drained = objects.iter().filter_map(Weak::upgrade).all(|object| {
                    object.invocations.get() == 0
                        && !states.iter().any(|state| {
                            root_alive(state)
                                && object
                                    .gate
                                    .borrow()
                                    .as_ref()
                                    .is_some_and(|allowed| allowed.contains(&state.local))
                        })
                });
                if drained {
                    return Ok(());
                }
                drop(registry);
                wait.cancelled().await;
            }
        }
        .boxed_local()
    }
    pub(crate) fn adopt_minimum(&self, version: u64) {
        for object in self.objects.iter().filter_map(Weak::upgrade) {
            object.min_code_version.set(version);
        }
    }
    pub(crate) fn open(mut self) {
        self.release();
    }
    pub(crate) fn open_silent(mut self) {
        self.release_inner(false);
    }
    fn release(&mut self) {
        self.release_inner(true);
    }
    fn release_inner(&mut self, notify: bool) {
        if !self.active {
            return;
        }
        self.active = false;
        for object in self.objects.iter().filter_map(Weak::upgrade) {
            object.gate.borrow_mut().take();
        }
        if notify && let Some(registry) = self.registry.upgrade() {
            registry.notify_change();
        }
    }
}
impl Drop for RootGate {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod tests;
