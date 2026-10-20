use super::*;
use crate::rpc::{
    CloseReason, Connection, ConnectionId, ConnectionOptions, ConnectionProfile,
    ConsumerLingerOptions, Link, RootOrigin,
    protocol::{BindRequest, ForwardedCaller, RootId, RootRef},
    remote::{IdleTicket, RemoteBinding},
    roots::RootEntry,
};

/// A normalized destination chosen from the consumer contract, never a wire override.
#[derive(Clone)]
pub struct RouteRequest {
    pub target: String,
    pub contract: Rc<Contract>,
    pub input: Value,
    pub(crate) source: Option<ConnectionId>,
    pub(crate) caller: Option<ForwardedCaller>,
}
impl RouteRequest {
    pub fn caller_principal(&self) -> Option<&Value> {
        self.caller.as_ref().map(|caller| &caller.principal)
    }
    pub fn origin(&self) -> Option<&str> {
        self.caller.as_ref().map(|caller| caller.origin.as_str())
    }
}
/// Resolves only explicitly configured remote routes; authorization is rechecked afterward.
pub type RouteResolver = Rc<dyn Fn(RouteRequest) -> LocalBoxFuture<'static, Result<Connection>>>;
/// Embedding policy called before and after suspended route resolution.
pub type RouteAuthorizer = Rc<dyn Fn(&RouteRequest) -> Result<()>>;

type PendingKey = (u64, String, String, String, String);
type FinalKey = (ConnectionId, String, String, String, String);
struct IdleEntry {
    id: u64,
    binding: Weak<RemoteBinding>,
    ticket: Rc<IdleTicket>,
}
#[derive(Default)]
pub(super) struct RpcState {
    reload: Rc<crate::rpc::reload::Administration>,
    next_connection: Cell<u64>,
    connections: RefCell<HashMap<ConnectionId, Connection>>,
    routes: RefCell<HashMap<String, Connection>>,
    resolver: RefCell<Option<RouteResolver>>,
    authorizer: RefCell<Option<RouteAuthorizer>>,
    generation: Cell<u64>,
    resolving: Cell<usize>,
    resolving_by_connection: RefCell<HashMap<ConnectionId, usize>>,
    remotes: RefCell<HashMap<u64, Rc<RemoteBinding>>>,
    provisional: RefCell<HashMap<PendingKey, Weak<RemoteBinding>>>,
    final_bindings: RefCell<HashMap<FinalKey, Weak<RemoteBinding>>>,
    revoked: RefCell<HashMap<(ConnectionId, String, String), u64>>,
    consumer_linger: ConsumerLingerOptions,
    idle: RefCell<std::collections::VecDeque<IdleEntry>>,
}

impl RpcState {
    pub(super) fn new(consumer_linger: ConsumerLingerOptions) -> Self {
        Self {
            consumer_linger,
            ..Self::default()
        }
    }
}

impl Host {
    /// Installs this Host's RPC incarnation once; does not authenticate any peer.
    ///
    /// # Errors
    /// Refuses a different name or replacing an already installed identity.
    pub fn set_rpc_identity(&self, identity: RootOrigin) -> Result<()> {
        self.inner.roots.set_identity(identity)
    }

    /// Accepts a peer authenticated by the embedding as an untrusted client.
    ///
    /// # Errors
    /// Refuses closed Hosts, invalid limits or exhausted connection admission.
    pub fn accept_client(
        &self,
        link: Link,
        authenticated_principal: Value,
        options: ConnectionOptions,
    ) -> Result<Connection> {
        self.inner.rpc_connect(
            link,
            ConnectionProfile::AcceptedClient,
            authenticated_principal,
            options,
        )
    }
    /// Connects as a client; the remote ingress determines the effective caller principal.
    ///
    /// # Errors
    /// Refuses invalid configuration or exhausted connection admission. Handshake is asynchronous.
    pub fn connect_client(
        &self,
        link: Link,
        expected_provider_name: impl Into<String>,
        options: ConnectionOptions,
    ) -> Result<Connection> {
        self.inner.rpc_connect(
            link,
            ConnectionProfile::ClientToProvider {
                expected: expected_provider_name.into(),
            },
            self.inner.principal.clone(),
            options,
        )
    }
    /// Accepts a configured child host with an embedding-authenticated principal.
    ///
    /// # Errors
    /// Refuses missing RPC identity, invalid configuration and exhausted admission.
    pub fn accept_child(
        &self,
        link: Link,
        expected_child_name: impl Into<String>,
        authenticated_principal: Value,
        options: ConnectionOptions,
    ) -> Result<Connection> {
        self.inner.rpc_connect(
            link,
            ConnectionProfile::AcceptedChild {
                expected: expected_child_name.into(),
            },
            authenticated_principal,
            options,
        )
    }
    /// Connects to a configured trusted parent, independently of its hello assertions.
    ///
    /// # Errors
    /// Refuses missing RPC identity, invalid configuration and exhausted admission.
    pub fn connect_parent(
        &self,
        link: Link,
        expected_parent_name: impl Into<String>,
        authenticated_principal: Value,
        options: ConnectionOptions,
    ) -> Result<Connection> {
        self.inner.rpc_connect(
            link,
            ConnectionProfile::TrustedParent {
                expected: expected_parent_name.into(),
            },
            authenticated_principal,
            options,
        )
    }

    /// Sets one explicit outgoing route; placement still comes from each contract/input.
    ///
    /// # Errors
    /// Refuses empty/self targets, another Host's connection and closed Hosts. Existing remote
    /// views are re-held without replaying calls; cached state can remain readable while Loading.
    ///
    /// ```
    /// use std::{cell::Cell as Counter, collections::BTreeMap, rc::Rc};
    /// use futures::FutureExt;
    /// use serde_json::json;
    /// use tembra_core::*;
    /// use contracts::{Contract, Member, Placement, ValueSchema};
    /// use rpc::{ConnectionOptions, MemoryLinkOptions, RootOrigin, memory_link};
    /// let driver = ManualDriver::new();
    /// let server = Host::new(HostOptions::new("server", json!("service")), driver.clone());
    /// let client = Host::new(HostOptions::new("client", json!("ui")), driver.clone());
    /// server.set_rpc_identity(RootOrigin::new("server", "11111111111111111111111111111111")?)?;
    /// let token = Rc::new(Contract::remote("answer", 1, json!({}),
    ///     Placement::Named { name: "server".into() }, BTreeMap::from([
    ///         ("main".into(), BTreeMap::from([("read".into(), Member::Method {
    ///             args: None, result: Some(ValueSchema::Portable(json!({"type":"integer"}))),
    ///         })])),
    ///     ]))?);
    /// let effects = Rc::new(Counter::new(0));
    /// let observed = effects.clone();
    /// server.serve(token.clone(), Rc::new(move |_, _, _| {
    ///     let observed = observed.clone();
    ///     Ok(Implementation::new().variant("main").json_method("read", move |_, _| {
    ///         observed.set(observed.get() + 1);
    ///         async { Ok(Some(json!(42))) }.boxed_local()
    ///     }))
    /// }), ServeOptions::default())?;
    /// let (left, right, _control) = memory_link(driver.clone(), MemoryLinkOptions::default())?;
    /// let outbound = client.connect_client(left, "server", ConnectionOptions::default())?;
    /// let _inbound = server.accept_client(
    ///     right, json!("accepted-user"), ConnectionOptions::default())?;
    /// client.set_route("server", outbound)?;
    /// driver.run_until(async {
    ///     let reference = client.get(token, json!({}), GetOptions::default()).await?;
    ///     assert_eq!(reference.call_json("read", None, None).await?, Some(json!(42)));
    ///     assert_eq!(effects.get(), 1);
    ///     let invalid = reference.call_json("read", Some(json!(null)), None).await.unwrap_err();
    ///     assert_eq!(invalid.reason, "args");
    ///     assert_eq!(effects.get(), 1); // Refused presence did not reach the provider.
    ///     client.close().await?;
    ///     server.close().await
    /// })?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn set_route(&self, target: impl Into<String>, connection: Connection) -> Result<()> {
        let target = target.into();
        if self.inner.closing.get() {
            return Err(RuntimeError::unavailable("closed", "Host is closing"));
        }
        if target.is_empty() || target == self.name() || !connection.belongs_to(self.id()) {
            return Err(RuntimeError::invalid(
                "route",
                "route must target another host through this Host's connection",
            ));
        }
        let old = self
            .inner
            .rpc
            .routes
            .borrow_mut()
            .insert(target.clone(), connection);
        drop(old);
        self.inner
            .rpc
            .generation
            .set(self.inner.rpc.generation.get() + 1);
        let remotes: Vec<_> = self
            .inner
            .rpc
            .remotes
            .borrow()
            .values()
            .filter(|remote| remote.target == target)
            .cloned()
            .collect();
        for remote in remotes {
            remote.reroute();
        }
        Ok(())
    }
    /// Installs an async resolver; ingress relays still require an explicit authorizer.
    /// Started resolutions retain their admission slots until their futures settle, including
    /// after the last interested reference is released. Release does not terminate a resolver.
    ///
    /// # Errors
    /// Refuses a closing Host. Host-owned acquisitions may use installed routes directly;
    /// ingress relays require an explicit authorizer, checked before and after resolution.
    /// An egress that cannot preserve the caller is refused even when policy allows it.
    pub fn set_resolver(&self, resolver: RouteResolver) -> Result<()> {
        if self.inner.closing.get() {
            return Err(RuntimeError::unavailable("closed", "Host is closing"));
        }
        let previous = self.inner.rpc.resolver.borrow_mut().replace(resolver);
        drop(previous);
        self.inner
            .rpc
            .generation
            .set(self.inner.rpc.generation.get() + 1);
        Ok(())
    }
    /// Installs embedding authorization for normalized routes, including accepted provenance.
    ///
    /// # Errors
    /// Refuses a closing Host. Policy refusal before admission executes no provider handler.
    pub fn set_route_authorizer(&self, authorizer: RouteAuthorizer) -> Result<()> {
        if self.inner.closing.get() {
            return Err(RuntimeError::unavailable("closed", "Host is closing"));
        }
        let previous = self.inner.rpc.authorizer.borrow_mut().replace(authorizer);
        drop(previous);
        self.inner
            .rpc
            .generation
            .set(self.inner.rpc.generation.get() + 1);
        Ok(())
    }
}

impl HostInner {
    pub(crate) fn reload_administration(&self) -> Rc<crate::rpc::reload::Administration> {
        self.rpc.reload.clone()
    }
    pub(crate) fn rpc_reload_stopped(&self) -> LocalBoxFuture<'static, ()> {
        self.rpc.reload.settled()
    }
    pub(crate) fn reload_current_version(&self) -> u64 {
        self.epochs.current().version.get()
    }
    pub(crate) fn reload_children(&self) -> Vec<Connection> {
        self.rpc
            .connections
            .borrow()
            .values()
            .filter(|connection| {
                matches!(
                    connection.profile(),
                    ConnectionProfile::AcceptedChild { .. }
                )
            })
            .cloned()
            .collect()
    }
    pub(crate) fn reconcile_reload_child(self: &Rc<Self>, connection: Connection) {
        crate::rpc::reload::reconcile_child(self.clone(), connection);
    }
    pub(crate) fn report_reload_background(&self, error: RuntimeError) {
        self.errors.borrow_mut().push(error);
    }
    pub(crate) fn has_reload_parent(&self) -> bool {
        self.rpc.connections.borrow().values().any(|connection| {
            matches!(
                connection.profile(),
                ConnectionProfile::TrustedParent { .. }
            ) && connection.status() != crate::rpc::ConnectionStatus::Closed
        })
    }

    pub(crate) fn requires_reload_coordination(&self) -> bool {
        self.rpc.connections.borrow().values().any(|connection| {
            connection.profile().trusted()
                && connection.status() != crate::rpc::ConnectionStatus::Closed
        })
    }

    pub(crate) fn next_root_id(&self) -> u64 {
        let id = self.next_root.get() + 1;
        self.next_root.set(id);
        id
    }
    pub(crate) fn rpc_identity(&self) -> Result<RootOrigin> {
        self.roots.identity()
    }
    pub(crate) fn rpc_connection(&self, id: ConnectionId) -> Option<Connection> {
        self.rpc.connections.borrow().get(&id).cloned()
    }
    fn rpc_connect(
        self: &Rc<Self>,
        link: Link,
        profile: ConnectionProfile,
        principal: Value,
        options: ConnectionOptions,
    ) -> Result<Connection> {
        if self.closing.get() {
            return Err(RuntimeError::unavailable("closed", "Host is closing"));
        }
        if profile.trusted()
            && self.reload_control.borrow().is_some()
            && !self.rpc.reload.is_coordinating()
        {
            return Err(RuntimeError::unavailable(
                "reload_busy",
                "trusted membership cannot join an admitted local or participant reload",
            ));
        }
        if self.rpc.connections.borrow().len() >= self.rpc_limits.max_connections {
            return Err(RuntimeError::unavailable(
                "too_many_connections",
                "Host connection budget reached",
            ));
        }
        tembra_contracts::validate(&serde_json::json!({}), &principal).map_err(contract_error)?;
        if !matches!(profile, ConnectionProfile::ClientToProvider { .. }) {
            self.rpc_identity()?;
        }
        let id = ConnectionId::new(
            self.rpc
                .next_connection
                .get()
                .checked_add(1)
                .ok_or_else(|| RuntimeError::unavailable("rpc_id", "connection IDs exhausted"))?,
        )?;
        self.rpc.next_connection.set(id.get());
        let connection =
            Connection::new(Rc::downgrade(self), id, link, profile, principal, options)?;
        self.rpc
            .connections
            .borrow_mut()
            .insert(id, connection.clone());
        Ok(connection)
    }
    pub(crate) fn rpc_route_generation(&self) -> u64 {
        self.rpc.generation.get()
    }
    pub(crate) fn rpc_is_closing(&self) -> bool {
        self.closing.get()
    }
    pub(crate) fn rpc_has_resolver(&self) -> bool {
        self.rpc.resolver.borrow().is_some()
    }
    pub(crate) fn rpc_route_request(
        &self,
        target: String,
        contract: Rc<Contract>,
        input: Value,
        source: Option<ConnectionId>,
        caller: Option<ForwardedCaller>,
    ) -> RouteRequest {
        RouteRequest {
            target,
            contract,
            input,
            source,
            caller,
        }
    }
    fn rpc_authorize(&self, request: &RouteRequest) -> Result<()> {
        if self.closing.get() {
            return Err(RuntimeError::unavailable("closed", "Host is closing"));
        }
        if let (Some(source), Some(caller)) = (request.source, &request.caller) {
            if !self.rpc.connections.borrow().contains_key(&source) {
                return Err(RuntimeError::new(
                    ErrorCode::Forbidden,
                    "origin_closed",
                    "route origin connection closed",
                ));
            }
            let principal =
                tembra_contracts::canonical_json(&caller.principal).map_err(contract_error)?;
            if self
                .rpc
                .revoked
                .borrow()
                .get(&(source, principal, caller.origin.clone()))
                .is_some_and(|generation| caller.authorization_generation.get() <= *generation)
            {
                return Err(RuntimeError::new(
                    ErrorCode::Forbidden,
                    "principal_invalidated",
                    "route origin grant was revoked",
                ));
            }
        }
        let authorizer = self.rpc.authorizer.borrow().clone();
        if let Some(authorizer) = authorizer {
            authorizer(request)?;
        } else if request.source.is_some() {
            return Err(RuntimeError::new(
                ErrorCode::Forbidden,
                "relay_authorizer",
                "relaying ingress requests requires an explicit route authorizer",
            ));
        }
        Ok(())
    }
    pub(crate) fn rpc_resolve(
        self: &Rc<Self>,
        request: RouteRequest,
    ) -> LocalBoxFuture<'static, Result<Connection>> {
        let first = self.rpc_authorize(&request);
        let generation = self.rpc.generation.get();
        let route = self.rpc.routes.borrow().get(&request.target).cloned();
        let static_route = route.is_some();
        let resolver = self.rpc.resolver.borrow().clone();
        let source = request.source;
        let source_admitted = source.is_none_or(|source| {
            self.rpc_connection(source).is_some_and(|connection| {
                self.rpc
                    .resolving_by_connection
                    .borrow()
                    .get(&source)
                    .copied()
                    .unwrap_or(0)
                    < connection.limits().max_pending_routes
            })
        });
        let admitted =
            self.rpc.resolving.get() < self.rpc_limits.max_pending_routes && source_admitted;
        if first.is_ok() && admitted {
            self.rpc.resolving.set(self.rpc.resolving.get() + 1);
            if let Some(source) = source {
                *self
                    .rpc
                    .resolving_by_connection
                    .borrow_mut()
                    .entry(source)
                    .or_default() += 1;
            }
        }
        let host = Rc::downgrade(self);
        let admission = (first.is_ok() && admitted).then(|| ResolutionLease {
            host: host.clone(),
            connections: source.into_iter().collect(),
        });
        async move {
            let mut admission = admission;
            first?;
            if !admitted {
                return Err(RuntimeError::unavailable(
                    "too_many_routes",
                    "Host or ingress connection resolution budget reached",
                ));
            }

            let connection = if let Some(route) = route {
                route
            } else if let Some(resolver) = resolver {
                resolver(request.clone()).await?
            } else {
                return Err(RuntimeError::unavailable(
                    "route",
                    "no explicit route to target",
                ));
            };
            if let Some(admission) = &mut admission {
                admission.attach(&connection)?;
            }
            if request.caller.is_some()
                && !matches!(
                    connection.profile(),
                    ConnectionProfile::AcceptedChild { .. }
                )
            {
                return Err(RuntimeError::new(
                    ErrorCode::Forbidden,
                    "relay_egress",
                    "the outgoing connection cannot preserve a forwarded caller",
                ));
            }
            if static_route && connection.status() == crate::rpc::ConnectionStatus::Closed {
                let error = connection.closed().await.error;
                if matches!(
                    error.code,
                    ErrorCode::UpgradeRequired
                        | ErrorCode::Invalid
                        | ErrorCode::Forbidden
                        | ErrorCode::ContractMismatch
                ) {
                    return Err(error);
                }
                return Err(RuntimeError::unavailable(
                    "route_disconnected",
                    "the static route is disconnected; a fresh route is required",
                ));
            }
            connection.ready().await?;
            let host = host.upgrade().ok_or_else(|| {
                RuntimeError::unavailable("closed", "Host disappeared during resolution")
            })?;
            if !connection.belongs_to(host.id) || generation != host.rpc.generation.get() {
                return Err(RuntimeError::new(
                    ErrorCode::Forbidden,
                    "route_changed",
                    "route changed during resolution",
                ));
            }
            host.rpc_authorize(&request)?;
            Ok(connection)
        }
        .boxed_local()
    }
    pub(crate) fn rpc_staged_bytes(&self) -> usize {
        self.rpc
            .remotes
            .borrow()
            .values()
            .map(|remote| remote.rpc_staged_bytes())
            .sum()
    }
    pub(crate) fn rpc_hold_remote(
        self: &Rc<Self>,
        contract: Rc<Contract>,
        input: Value,
        state: bool,
        source: Option<ConnectionId>,
        caller: Option<ForwardedCaller>,
    ) -> Result<Reference> {
        let target = contract
            .resolve_host(&input)
            .map_err(contract_error)?
            .ok_or_else(|| {
                RuntimeError::invalid("placement", "remote contract has no destination")
            })?;
        let caller_key = caller_key(source, caller.as_ref())?;
        let key = (
            self.rpc.generation.get(),
            target.clone(),
            contract.identity(&input).map_err(contract_error)?,
            contract.fingerprint().into(),
            caller_key,
        );
        let existing = self
            .rpc
            .provisional
            .borrow()
            .get(&key)
            .and_then(Weak::upgrade);
        if let Some(existing) = existing
            && existing.reusable()
        {
            return Ok(Reference::remote(self, existing, contract, state));
        }
        while self.rpc.remotes.borrow().len() >= self.rpc_limits.max_remote_holds {
            if !self.rpc_evict_idle() {
                return Err(RuntimeError::unavailable(
                    "too_many_holds",
                    "Host remote binding budget reached",
                ));
            }
        }
        let id = self.next_object.get() + 1;
        self.next_object.set(id);
        let binding = RemoteBinding::new(self, id, target, contract.clone(), input, source, caller);
        self.rpc.remotes.borrow_mut().insert(id, binding.clone());
        self.rpc
            .provisional
            .borrow_mut()
            .insert(key, Rc::downgrade(&binding));
        Ok(Reference::remote(self, binding, contract, state))
    }
    pub(crate) fn rpc_coalesce_remote(
        &self,
        binding: &Rc<RemoteBinding>,
        realm: ConnectionId,
    ) -> Option<Rc<RemoteBinding>> {
        let key = (
            realm,
            binding.target.clone(),
            binding.contract.identity(&binding.input).ok()?,
            binding.contract.fingerprint().into(),
            binding.caller_key().ok()?,
        );
        let current = self
            .rpc
            .final_bindings
            .borrow()
            .get(&key)
            .and_then(Weak::upgrade);
        let current = current.filter(|current| current.id() != binding.id() && current.reusable());
        if current.is_none() {
            self.rpc
                .final_bindings
                .borrow_mut()
                .insert(key, Rc::downgrade(binding));
        }
        current
    }
    pub(crate) fn rpc_idle_remote(&self, binding: Rc<RemoteBinding>) -> bool {
        let policy = &self.rpc.consumer_linger;
        let ttl = policy.ttl(binding.contract.name());
        if self.closing.get() || policy.capacity() == 0 || ttl.is_zero() || !binding.applied_shape()
        {
            return false;
        }
        let Some(deadline) = self.driver.now().checked_add(ttl) else {
            return false;
        };
        let ticket = IdleTicket::new(deadline);
        binding.begin_idle(ticket.clone());
        self.rpc.idle.borrow_mut().push_back(IdleEntry {
            id: binding.id(),
            binding: Rc::downgrade(&binding),
            ticket,
        });
        while self.rpc.idle.borrow().len() > policy.capacity() {
            self.rpc_evict_idle();
        }
        true
    }

    pub(crate) fn rpc_remove_idle(&self, id: u64) {
        let removed = {
            let mut idle = self.rpc.idle.borrow_mut();
            idle.iter()
                .position(|entry| entry.id == id)
                .and_then(|index| idle.remove(index))
        };
        if let Some(entry) = removed {
            entry.ticket.cancel();
        }
    }

    fn rpc_evict_idle(&self) -> bool {
        let entry = self.rpc.idle.borrow_mut().pop_front();
        if let Some(entry) = entry {
            if let Some(binding) = entry.binding.upgrade() {
                binding.expire_idle(&entry.ticket);
            }
            entry.ticket.cancel();
            true
        } else {
            false
        }
    }

    pub(crate) fn rpc_evict_idle_on(&self, connection: ConnectionId) -> bool {
        let candidates = self
            .rpc
            .idle
            .borrow()
            .iter()
            .map(|entry| (entry.id, entry.binding.clone(), entry.ticket.clone()))
            .collect::<Vec<_>>();
        for (id, weak, ticket) in candidates {
            let Some(binding) = weak.upgrade() else {
                continue;
            };
            if binding.idle_connection() != Some(connection) {
                continue;
            }
            let removed = {
                let mut idle = self.rpc.idle.borrow_mut();
                idle.iter()
                    .position(|entry| entry.id == id && Rc::ptr_eq(&entry.ticket, &ticket))
                    .and_then(|index| idle.remove(index))
            };
            if let Some(entry) = removed {
                // Forget the exact interval before Release, cancellation wakes or user reentry.
                binding.expire_idle(&entry.ticket);
                entry.ticket.cancel();
                return true;
            }
        }
        false
    }

    pub(crate) fn rpc_forget_remote(&self, id: u64) {
        self.rpc_remove_idle(id);
        let removed = self.rpc.remotes.borrow_mut().remove(&id);
        self.rpc
            .provisional
            .borrow_mut()
            .retain(|_, remote| remote.upgrade().is_some_and(|remote| remote.id() != id));
        self.rpc
            .final_bindings
            .borrow_mut()
            .retain(|_, remote| remote.upgrade().is_some_and(|remote| remote.id() != id));
        drop(removed);
    }
    pub(crate) fn rpc_hold(
        self: &Rc<Self>,
        request: BindRequest,
        principal: Value,
        source: ConnectionId,
    ) -> Result<Reference> {
        if self.closing.get() {
            return Err(RuntimeError::unavailable("closed", "Host is closing"));
        }
        if request.consumer.kind() != ContractKind::Remote {
            return Err(RuntimeError::new(
                ErrorCode::Forbidden,
                "local_contract",
                "local contracts cannot cross RPC",
            ));
        }
        let input = request
            .consumer
            .normalize_input(&request.input)
            .map_err(contract_error)?;
        if request
            .consumer
            .resolve_host(&input)
            .map_err(contract_error)?
            .as_deref()
            != Some(request.target.as_str())
        {
            return Err(RuntimeError::invalid(
                "placement",
                "wire target differs from contract placement",
            ));
        }
        if request.target == self.name {
            self.hold(
                request.consumer,
                input,
                GetOptions {
                    principal: Some(principal),
                    state: request.state,
                },
            )
        } else {
            self.rpc_hold_remote(
                request.consumer,
                input,
                request.state,
                Some(source),
                request.caller,
            )
        }
    }
    pub(crate) fn rpc_enter(
        self: &Rc<Self>,
        root: Option<&RootRef>,
        carrier: ConnectionId,
        principal: Value,
    ) -> Result<RootEntry> {
        if self.closing.get() {
            return Err(RuntimeError::unavailable("closed", "Host is closing"));
        }
        self.roots.enter(root, carrier, principal)
    }
    pub(crate) fn rpc_export(&self, context: &Context, link: ConnectionId) -> Result<RootRef> {
        self.roots.export(context, link)
    }
    pub(crate) fn rpc_root_end(&self, link: ConnectionId, root: &RootId) {
        self.roots.carrier_end(link, root);
    }
    pub(crate) fn rpc_link_closed(&self, link: ConnectionId, reason: RuntimeError) {
        self.roots.link_closed(link, reason.clone());
        let removed = self.rpc.connections.borrow_mut().remove(&link);
        drop(removed);
        let remotes: Vec<_> = self.rpc.remotes.borrow().values().cloned().collect();
        for remote in remotes {
            if remote.source() == Some(link) {
                remote.fail(reason.clone());
            }
        }
        self.rpc
            .revoked
            .borrow_mut()
            .retain(|(connection, _, _), _| *connection != link);
    }
    pub(crate) fn rpc_invalidate_caller(
        &self,
        source: ConnectionId,
        principal: &Value,
        origin: &str,
        through_generation: u64,
    ) -> Result<()> {
        let key = tembra_contracts::canonical_json(principal).map_err(contract_error)?;
        let proposed = (source, key.clone(), origin.to_owned());
        let overflow = {
            let revoked = self.rpc.revoked.borrow();
            if revoked.contains_key(&proposed) {
                false
            } else {
                let bytes: usize = revoked
                    .keys()
                    .map(|(_, principal, origin)| principal.len() + origin.len())
                    .sum();
                revoked.len() >= self.rpc_limits.max_revocation_history
                    || bytes
                        .checked_add(key.len())
                        .and_then(|bytes| bytes.checked_add(origin.len()))
                        .is_none_or(|bytes| bytes > self.rpc_limits.max_revocation_bytes)
            }
        };
        if overflow {
            return Err(RuntimeError::unavailable(
                "revocation_limit",
                "Host revocation history budget reached",
            ));
        }
        self.rpc
            .revoked
            .borrow_mut()
            .entry((source, key, origin.to_owned()))
            .and_modify(|generation| *generation = (*generation).max(through_generation))
            .or_insert(through_generation);
        let remotes: Vec<_> = self.rpc.remotes.borrow().values().cloned().collect();
        for remote in remotes {
            if remote.matches_caller(source, principal, origin, through_generation) {
                remote.fail(RuntimeError::new(
                    ErrorCode::Forbidden,
                    "principal_invalidated",
                    "forwarded origin grant revoked",
                ));
            }
        }
        Ok(())
    }
    pub(super) fn rpc_begin_close(&self) {
        let reason = RuntimeError::new(ErrorCode::Gone, "host_closed", "Host closed");
        self.rpc.reload.stop(reason.clone());
        while self.rpc_evict_idle() {}
        let connections: Vec<_> = self.rpc.connections.borrow().values().cloned().collect();
        for connection in connections {
            connection.reload_stopping(reason.clone());
            connection.cancel_ingress(reason.clone());
        }
    }
    pub(super) fn rpc_fail_disconnected_cleanup(&self) {
        let remotes: Vec<_> = self.rpc.remotes.borrow().values().cloned().collect();
        for remote in remotes {
            remote.close_disconnected_dependency();
        }
    }
    pub(super) fn rpc_close_links(&self) {
        // Held dependencies and cleanup-created roots remain usable until resource cleanup
        // finishes. The closing admission gate already refuses new holds and incoming calls.
        let reason = RuntimeError::new(ErrorCode::Gone, "host_closed", "Host closed");
        self.roots.close(reason.clone());
        let remotes: Vec<_> = self.rpc.remotes.borrow().values().cloned().collect();
        for remote in remotes {
            remote.fail(reason.clone());
        }
        let connections: Vec<_> = self.rpc.connections.borrow().values().cloned().collect();
        for connection in connections {
            connection.close(CloseReason::closed());
        }
    }
}
struct ResolutionLease {
    host: Weak<HostInner>,
    connections: Vec<ConnectionId>,
}
impl ResolutionLease {
    fn attach(&mut self, connection: &Connection) -> Result<()> {
        let id = connection.id();
        if self.connections.contains(&id) {
            return Ok(());
        }
        let host = self.host.upgrade().ok_or_else(|| {
            RuntimeError::unavailable("closed", "Host disappeared during resolution")
        })?;
        if !connection.belongs_to(host.id) {
            return Err(RuntimeError::new(
                ErrorCode::Forbidden,
                "route_changed",
                "resolver returned another Host's connection",
            ));
        }
        let mut pending = host.rpc.resolving_by_connection.borrow_mut();
        let count = pending.entry(id).or_default();
        if *count >= connection.limits().max_pending_routes {
            return Err(RuntimeError::unavailable(
                "too_many_routes",
                "outgoing connection resolution budget reached",
            ));
        }
        *count += 1;
        self.connections.push(id);
        Ok(())
    }
}
impl Drop for ResolutionLease {
    fn drop(&mut self) {
        if let Some(host) = self.host.upgrade() {
            host.rpc
                .resolving
                .set(host.rpc.resolving.get().saturating_sub(1));
            let mut sources = host.rpc.resolving_by_connection.borrow_mut();
            for source in &self.connections {
                if let Some(count) = sources.get_mut(source) {
                    *count -= 1;
                    if *count == 0 {
                        sources.remove(source);
                    }
                }
            }
        }
    }
}
fn caller_key(source: Option<ConnectionId>, caller: Option<&ForwardedCaller>) -> Result<String> {
    caller.map_or(Ok(String::new()), |caller| {
        tembra_contracts::canonical_json(&serde_json::json!([
            source.map(ConnectionId::get),
            caller.principal,
            caller.origin,
            caller.authorization_generation.get()
        ]))
        .map_err(contract_error)
    })
}
