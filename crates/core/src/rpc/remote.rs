mod linger;
mod replica;
pub(crate) use linger::IdleTicket;

use super::{Connection, ConnectionStatus, protocol::*};
use crate::{
    Context, ErrorCode, RefStatus, Result, RuntimeError, Shape, Snapshot, StateEvent,
    host::HostInner, reference::Claim, state::ReadPort,
};
use futures::{FutureExt, future::LocalBoxFuture};
use serde_json::Value;
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::{Rc, Weak},
};
use tembra_contracts::{Contract, Member, ValueSchema};

struct Attachment {
    connection: ConnectionId,
    hold: HoldId,
    generation: AttachmentId,
}
struct Unit {
    frame: ShapeFrame,
    provider: Rc<Contract>,
    staged: HashMap<String, Snapshot>,
    bytes: usize,
}
struct Current {
    attachment: AttachmentId,
    provider: Rc<Contract>,
    mode: BindingMode,
    shape: Shape,
    unit: UnitId,
    streams: HashMap<String, StreamId>,
}

enum Terminal {
    Released,
    Failed(RuntimeError),
}

pub(crate) struct RemoteBinding {
    weak: Weak<Self>,
    pub(crate) host: Weak<HostInner>,
    id: u64,
    pub(crate) target: String,
    pub(crate) contract: Rc<Contract>,
    pub(crate) input: Value,
    pub(crate) route_generation: Cell<u64>,
    source: Option<ConnectionId>,
    caller: Option<ForwardedCaller>,
    resolving: Cell<bool>,
    idle: RefCell<Option<Rc<IdleTicket>>>,
    claims: RefCell<Vec<Weak<Claim>>>,
    attachment: RefCell<Option<Attachment>>,
    next_attachment: Cell<u64>,
    requested: Cell<bool>,
    connected: Cell<bool>,
    hydrated: Cell<bool>,
    terminal: RefCell<Option<Terminal>>,
    pending: RefCell<Option<Unit>>,
    current: RefCell<Option<Current>>,
    replicas: RefCell<HashMap<String, Rc<ReadPort>>>,
    delegate: RefCell<Option<Rc<Self>>>,
    recoveries: RefCell<HashMap<String, usize>>,
}

impl RemoteBinding {
    pub(crate) fn new(
        host: &Rc<HostInner>,
        id: u64,
        target: String,
        contract: Rc<Contract>,
        input: Value,
        source: Option<ConnectionId>,
        caller: Option<ForwardedCaller>,
    ) -> Rc<Self> {
        Rc::new_cyclic(|weak| Self {
            weak: weak.clone(),
            host: Rc::downgrade(host),
            id,
            target,
            contract,
            input,
            source,
            caller,
            route_generation: Cell::new(host.rpc_route_generation()),
            resolving: Cell::new(false),
            idle: RefCell::new(None),
            claims: RefCell::new(Vec::new()),
            attachment: RefCell::new(None),
            next_attachment: Cell::new(0),
            requested: Cell::new(false),
            connected: Cell::new(false),
            hydrated: Cell::new(false),
            terminal: RefCell::new(None),
            pending: RefCell::new(None),
            current: RefCell::new(None),
            replicas: RefCell::new(HashMap::new()),
            delegate: RefCell::new(None),
            recoveries: RefCell::new(HashMap::new()),
        })
    }
    pub(crate) fn id(&self) -> u64 {
        self.id
    }
    pub(crate) fn error(&self) -> Option<RuntimeError> {
        if let Some(delegate) = self.delegate.borrow().clone() {
            return delegate.error();
        }
        match self.terminal.borrow().as_ref() {
            Some(Terminal::Failed(error)) => Some(error.clone()),
            Some(Terminal::Released) | None => None,
        }
    }
    pub(crate) fn provider_view(&self) -> Result<(Rc<Contract>, BindingMode)> {
        if let Some(delegate) = self.delegate.borrow().clone() {
            return delegate.provider_view();
        }
        self.current
            .borrow()
            .as_ref()
            .map(|current| (current.provider.clone(), current.mode.clone()))
            .ok_or_else(|| {
                RuntimeError::new(
                    ErrorCode::NotReady,
                    "shape",
                    "remote provider view is not ready",
                )
            })
    }
    pub(crate) fn shape(&self) -> Option<Shape> {
        if let Some(delegate) = self.delegate.borrow().clone() {
            return delegate.shape();
        }
        self.current
            .borrow()
            .as_ref()
            .map(|current| current.shape.clone())
    }
    fn claims(&self) -> Vec<Rc<Claim>> {
        let mut claims = self.claims.borrow_mut();
        claims.retain(|claim| claim.strong_count() > 0);
        claims
            .iter()
            .filter_map(Weak::upgrade)
            .filter(|claim| {
                !matches!(
                    claim.status_for_remote(),
                    RefStatus::Released | RefStatus::Gone
                )
            })
            .collect()
    }
    pub(crate) fn add_claim(&self, claim: &Rc<Claim>) {
        if let Some(delegate) = self.delegate.borrow().clone() {
            delegate.add_claim(claim);
            return;
        }
        self.cancel_idle();
        self.claims.borrow_mut().push(Rc::downgrade(claim));
        if let Err(error) = self.interest_changed() {
            self.fail(error);
        }
        self.start_route();
    }
    pub(crate) fn claim_status(&self, claim: &Claim) -> RefStatus {
        if let Some(delegate) = self.delegate.borrow().clone() {
            return delegate.claim_status(claim);
        }
        if self.terminal.borrow().is_some() {
            return RefStatus::Gone;
        }
        if self.connected.get()
            && self.current.borrow().is_some()
            && (!claim.wants_state || self.hydrated.get())
        {
            RefStatus::Live
        } else {
            RefStatus::Loading
        }
    }
    pub(crate) fn sync_claim(&self, claim: &Rc<Claim>) {
        if let Some(delegate) = self.delegate.borrow().clone() {
            delegate.sync_claim(claim);
            return;
        }
        if let Some(error) = self.error() {
            claim.remote_end(error);
            return;
        }
        if self.claim_status(claim) == RefStatus::Live {
            let values: Vec<_> = self
                .replicas
                .borrow()
                .iter()
                .filter_map(|(name, port)| {
                    port.current()
                        .ok()
                        .map(|snapshot| (name.clone(), StateEvent::Snapshot(snapshot)))
                })
                .collect();
            let Some(host) = self.host.upgrade() else {
                return;
            };
            let hold = host.publication_queue.hold();
            if let Ok(ports) = claim.remote_candidates(&values) {
                let publications = ports
                    .into_iter()
                    .filter_map(|(port, event)| port.adopt(event))
                    .collect();
                host.publication_queue.append_batch(publications);
                claim.remote_set_ready(true);
                claim.remote_publish_ready();
            }
            drop(hold);
        } else {
            claim.remote_loading();
        }
    }
    pub(crate) fn claim_released(&self) {
        if let Some(delegate) = self.delegate.borrow().clone() {
            delegate.claim_released();
            return;
        }
        if self.claims().is_empty() {
            if self.idle_ticket().is_some() {
                return;
            }
            if let Some(host) = self.host.upgrade()
                && let Some(binding) = self.weak.upgrade()
                && host.rpc_idle_remote(binding)
            {
                return;
            }
            self.release_binding();
        } else if let Err(error) = self.interest_changed() {
            self.fail(error);
        }
    }
    pub(crate) fn interest_changed(&self) -> Result<()> {
        if let Some(delegate) = self.delegate.borrow().clone() {
            return delegate.interest_changed();
        }
        let claims = self.claims();
        if claims.is_empty() {
            return Ok(());
        }
        let wanted = claims.iter().any(|claim| claim.state_wanted());
        if self.requested.replace(wanted) != wanted {
            if wanted {
                self.hydrated.set(false);
            }
            let attachment = self
                .attachment
                .borrow()
                .as_ref()
                .map(|attachment| (attachment.connection, attachment.hold));
            if let Some((id, hold)) = attachment
                && let Some(connection) = self.connection(id)
            {
                connection.want_state(hold, wanted)?;
            }
        }
        Ok(())
    }
    pub(crate) fn start_route(&self) {
        if self.delegate.borrow().is_some()
            || self.attachment.borrow().is_some()
            || self.resolving.get()
            || self.terminal.borrow().is_some()
            || !self.owned()
        {
            return;
        }
        let Some(host) = self.host.upgrade() else {
            return;
        };
        self.resolving.set(true);
        let weak = self.weak.clone();
        let request = host.rpc_route_request(
            self.target.clone(),
            self.contract.clone(),
            self.input.clone(),
            self.source,
            self.caller.clone(),
        );
        let generation = host.rpc_route_generation();
        self.route_generation.set(generation);
        let route = host.rpc_resolve(request);
        host.driver.spawn(
            async move {
                let result = route.await;
                let Some(binding) = weak.upgrade() else {
                    return;
                };
                binding.resolving.set(false);
                let Some(host) = binding.host.upgrade() else {
                    return;
                };
                if !binding.owned() || host.rpc_is_closing() {
                    return;
                }
                if generation != host.rpc_route_generation() {
                    binding.start_route();
                    return;
                }
                match result {
                    Ok(connection) => {
                        if let Some(existing) =
                            host.rpc_coalesce_remote(&binding, connection.authority_realm())
                        {
                            let claims = binding.claims();
                            *binding.delegate.borrow_mut() = Some(existing.clone());
                            host.rpc_forget_remote(binding.id);
                            for claim in claims {
                                existing.add_claim(&claim);
                                existing.sync_claim(&claim);
                            }
                        } else {
                            if binding.claims().is_empty() {
                                // Optional idle reconnection cannot evict another idle attachment.
                                if !connection.has_hold_capacity() {
                                    binding.release_binding();
                                    return;
                                }
                            } else {
                                while !connection.has_hold_capacity() {
                                    if !host.rpc_evict_idle_on(connection.id()) {
                                        break;
                                    }
                                }
                            }
                            if let Err(error) = connection.bind(binding.weak.clone()) {
                                binding.fail(error);
                            }
                        }
                    }
                    Err(error)
                        if error.code == ErrorCode::Unavailable
                            && error.reason == "route_disconnected" =>
                    {
                        // A static route is replaced explicitly. A coalesced acquisition must
                        // not turn an existing disconnected binding into a terminal failure.
                        for claim in binding.claims() {
                            claim.remote_loading();
                        }
                    }
                    Err(error) => binding.fail(error),
                }
            }
            .boxed_local(),
        );
    }
    pub(crate) fn source(&self) -> Option<ConnectionId> {
        self.source
    }
    pub(crate) fn caller_key(&self) -> Result<String> {
        self.caller.as_ref().map_or(Ok(String::new()), |caller| {
            tembra_contracts::canonical_json(&serde_json::json!([
                self.source.map(ConnectionId::get),
                caller.principal,
                caller.origin,
                caller.authorization_generation.get()
            ]))
            .map_err(|error| RuntimeError::invalid(error.reason, error.message))
        })
    }
    pub(crate) fn matches_caller(
        &self,
        source: ConnectionId,
        principal: &Value,
        origin: &str,
        generation: u64,
    ) -> bool {
        self.source == Some(source)
            && self.caller.as_ref().is_some_and(|caller| {
                caller.principal == *principal
                    && caller.origin == origin
                    && caller.authorization_generation.get() <= generation
            })
    }
    pub(crate) fn reroute(&self) {
        let attachment = self.attachment.borrow_mut().take();
        if let Some(attachment) = attachment
            && let Some(connection) = self.connection(attachment.connection)
        {
            connection.release(attachment.hold);
        }
        self.pending.borrow_mut().take();
        self.connected.set(false);
        for claim in self.claims() {
            claim.remote_loading();
        }
        self.start_route();
    }
    pub(crate) fn rpc_attach(&self, connection: &Connection, hold: HoldId) -> Result<AttachmentId> {
        if self.terminal.borrow().is_some() || !self.owned() {
            return Err(RuntimeError::new(
                ErrorCode::Released,
                "released",
                "remote binding has no live claim",
            ));
        }
        let host = self.host.upgrade().ok_or_else(closed)?;
        if host.rpc_is_closing() || self.route_generation.get() != host.rpc_route_generation() {
            return Err(RuntimeError::new(
                ErrorCode::Forbidden,
                "route_changed",
                "route admission changed during resolution",
            ));
        }
        let generation = AttachmentId::new(
            self.next_attachment
                .get()
                .checked_add(1)
                .ok_or_else(closed)?,
        )?;
        self.next_attachment.set(generation.get());
        *self.attachment.borrow_mut() = Some(Attachment {
            connection: connection.id(),
            hold,
            generation,
        });
        self.connected.set(false);
        let claims = self.claims();
        if !claims.is_empty() {
            self.requested
                .set(claims.iter().any(|claim| claim.state_wanted()));
        }
        Ok(generation)
    }
    pub(crate) fn rpc_bind_data(&self, attachment: AttachmentId) -> Result<BindRequest> {
        self.check_attachment(attachment)?;
        Ok(BindRequest {
            target: self.target.clone(),
            consumer: self.contract.clone(),
            input: self.input.clone(),
            state: self.requested.get(),
            caller: self.caller.clone(),
        })
    }
    pub(crate) fn rpc_staged_bytes(&self) -> usize {
        self.pending.borrow().as_ref().map_or(0, |unit| unit.bytes)
    }
    pub(crate) fn rpc_gone(&self, attachment: AttachmentId, error: RuntimeError) {
        if self.attached(attachment) {
            self.fail(error);
        }
    }
    pub(crate) fn rpc_detached(&self, attachment: AttachmentId, error: RuntimeError) {
        if !self.attached(attachment) {
            return;
        }
        self.attachment.borrow_mut().take();
        self.pending.borrow_mut().take();
        self.connected.set(false);
        if matches!(
            error.code,
            ErrorCode::UpgradeRequired
                | ErrorCode::Invalid
                | ErrorCode::Forbidden
                | ErrorCode::ContractMismatch
        ) {
            self.fail(error);
            return;
        }
        for claim in self.claims() {
            claim.remote_loading();
        }
        if self
            .host
            .upgrade()
            .is_some_and(|host| host.rpc_has_resolver())
        {
            self.start_route();
        }
    }
    pub(crate) fn fail(&self, error: RuntimeError) {
        self.terminate(Terminal::Failed(error.clone()), error);
    }
    fn release_binding(&self) {
        self.terminate(
            Terminal::Released,
            RuntimeError::new(ErrorCode::Released, "released", "remote binding released"),
        );
    }
    fn terminate(&self, terminal: Terminal, error: RuntimeError) {
        if self.terminal.borrow().is_some() {
            return;
        }
        *self.terminal.borrow_mut() = Some(terminal);
        self.cancel_idle();
        if let Some(host) = self.host.upgrade() {
            host.rpc_forget_remote(self.id);
        }
        self.pending.borrow_mut().take();
        let attachment = self.attachment.borrow_mut().take();
        if let Some(attachment) = attachment
            && let Some(connection) = self.connection(attachment.connection)
        {
            connection.release(attachment.hold);
        }
        for claim in self.claims() {
            claim.remote_end(error.clone());
        }
        let ports: Vec<_> = self.replicas.borrow().values().cloned().collect();
        for port in ports {
            port.terminate(error.clone());
        }
    }
    fn attached(&self, attachment: AttachmentId) -> bool {
        self.attachment
            .borrow()
            .as_ref()
            .is_some_and(|current| current.generation == attachment)
    }
    fn check_attachment(&self, attachment: AttachmentId) -> Result<()> {
        if self.attached(attachment) {
            Ok(())
        } else {
            Err(RuntimeError::new(
                ErrorCode::Gone,
                "attachment",
                "remote attachment retired",
            ))
        }
    }
    fn connection(&self, id: ConnectionId) -> Option<Connection> {
        self.host.upgrade().and_then(|host| host.rpc_connection(id))
    }
    fn port(&self, name: &str) -> Result<Rc<ReadPort>> {
        if let Some(port) = self.replicas.borrow().get(name).cloned() {
            return Ok(port);
        }
        let host = self.host.upgrade().ok_or_else(closed)?;
        let weak = self.weak.clone();
        let read = Rc::new(move || {
            weak.upgrade()
                .ok_or_else(closed)?
                .error()
                .map_or(Ok(()), Err)
        });
        let report_host = self.host.clone();
        let port = ReadPort::new(
            read.clone(),
            read,
            Rc::new(move |error| {
                if let Some(host) = report_host.upgrade() {
                    host.errors.borrow_mut().push(error);
                }
            }),
            host.publication_queue.clone(),
        );
        self.replicas.borrow_mut().insert(name.into(), port.clone());
        Ok(port)
    }
    pub(crate) fn call(
        &self,
        name: &str,
        args: Option<Value>,
        context: &Context,
    ) -> LocalBoxFuture<'static, Result<Option<Value>>> {
        if let Some(delegate) = self.delegate.borrow().clone() {
            return delegate.call(name, args, context);
        }
        let result = (|| {
            let host = self.host.upgrade().ok_or_else(closed)?;
            context.check(host.id)?;
            let shape = self.shape().ok_or_else(|| {
                RuntimeError::new(ErrorCode::NotReady, "shape", "remote shape is pending")
            })?;
            let Some(Member::Method {
                args: schema,
                result,
            }) = self
                .contract
                .members(shape.variant.as_deref().unwrap_or(""))
                .and_then(|members| members.get(name))
            else {
                return Err(RuntimeError::new(
                    ErrorCode::NotInVariant,
                    "method",
                    "method is outside the consumer shape",
                ));
            };
            let args = match (schema, args) {
                (None, None) => None,
                (Some(ValueSchema::Portable(schema)), Some(value)) => Some(
                    tembra_contracts::clean_arguments(schema, &value)
                        .and_then(|value| {
                            tembra_contracts::validate(schema, &value)?;
                            Ok(value)
                        })
                        .map_err(|error| RuntimeError::invalid("args", error.message))?,
                ),
                (Some(ValueSchema::Native(_)), _) => {
                    return Err(RuntimeError::invalid(
                        "native_type",
                        "remote calls cannot carry native values",
                    ));
                }
                _ => {
                    return Err(RuntimeError::invalid(
                        "args",
                        "argument presence differs from consumer method",
                    ));
                }
            };
            let connection = self
                .attachment
                .borrow()
                .as_ref()
                .map(|current| (current.connection, current.hold))
                .ok_or_else(closed)?;
            let connection_handle = self.connection(connection.0).ok_or_else(closed)?;
            if connection_handle.status() != ConnectionStatus::Open {
                return Err(closed());
            }
            Ok((
                connection_handle.call(connection.1, name.into(), args, context),
                result.clone(),
            ))
        })();
        async move {
            let (call, result) = result?;
            let value = call.await?;
            match result {
                None => Ok(None),
                Some(ValueSchema::Portable(schema)) => {
                    let value = value.ok_or_else(|| {
                        RuntimeError::invalid("result", "provider omitted the consumer result")
                    })?;
                    tembra_contracts::validate(&schema, &value)
                        .map_err(|error| RuntimeError::invalid("result", error.message))?;
                    Ok(Some(value))
                }
                Some(ValueSchema::Native(_)) => Err(RuntimeError::invalid(
                    "native_type",
                    "native result cannot cross RPC",
                )),
            }
        }
        .boxed_local()
    }
}
fn closed() -> RuntimeError {
    RuntimeError::unavailable("closed", "remote connection or Host is unavailable")
}
