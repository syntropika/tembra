pub(crate) mod facets;
pub(crate) mod reload;
pub use reload::{ReloadOptions, ReloadReport};
mod limits;
mod rpc;
pub use limits::HostRpcLimits;

pub use rpc::{RouteAuthorizer, RouteRequest, RouteResolver};

use crate::object::{
    Declaration, Factory, Implementation, Lifetime, ObjectRecord, Phase, ServeOptions,
};
use crate::reference::Reference;
use crate::resource::{ResourceDependency, ResourceRecord};
use crate::state::{PublicationQueue, StateGraph};
use crate::{CancellationToken, Context, Driver, ErrorCode, Result, RuntimeError};
use futures::future::{FutureExt, LocalBoxFuture};
use serde_json::Value;
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    rc::{Rc, Weak},
};
use tembra_contracts::{Contract, ContractKind, Member};

thread_local! { static NEXT_HOST: Cell<u64> = const { Cell::new(1) }; }

#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct Address(pub String, pub String, pub String);

pub struct HostOptions {
    pub name: String,
    pub self_principal: Value,
    pub principal_key: Rc<dyn Fn(&Value) -> String>,
    pub ready_timeout: std::time::Duration,
    rpc_limits: HostRpcLimits,
    consumer_linger: crate::rpc::ConsumerLingerOptions,
}

impl HostOptions {
    pub fn new(name: impl Into<String>, self_principal: Value) -> Self {
        Self {
            name: name.into(),
            self_principal,
            principal_key: Rc::new(ToString::to_string),
            ready_timeout: std::time::Duration::from_secs(60),
            rpc_limits: HostRpcLimits::default(),
            consumer_linger: crate::rpc::ConsumerLingerOptions::default(),
        }
    }
    /// Installs a checked optional idle-consumer policy before Host construction.
    /// Idle attachments consume connection and provider hold/state budgets. Real demand can evict
    /// idle entries on its connection; active holds still retain their normal admission limits.
    pub fn with_consumer_linger(mut self, options: crate::rpc::ConsumerLingerOptions) -> Self {
        self.consumer_linger = options;
        self
    }

    /// Installs checked Host-wide RPC limits for subsequent infallible Host construction.
    ///
    /// # Errors
    /// Refuses zero limits or values outside the portable safe-integer domain.
    pub fn with_rpc_limits(mut self, limits: HostRpcLimits) -> Result<Self> {
        limits.validate()?;
        self.rpc_limits = limits;
        Ok(self)
    }
}

#[derive(Clone)]
pub struct GetOptions {
    pub principal: Option<Value>,
    /// Full state demand; false allows method readiness without state hydration.
    pub state: bool,
}
impl Default for GetOptions {
    fn default() -> Self {
        Self {
            principal: None,
            state: true,
        }
    }
}

#[derive(Clone)]
pub(crate) struct Service {
    pub(crate) contract: Rc<Contract>,
    pub(crate) factory: Factory,
    pub(crate) options: ServeOptions,
    pub(crate) ordinal: facets::InstalledServiceId,
    pub(crate) owner: Option<facets::FacetRunId>,
}

#[derive(Clone)]
pub struct Host {
    pub(crate) inner: Rc<HostInner>,
}

pub(crate) struct HostAttachment(pub Weak<HostInner>);
struct InitializingObject(Weak<ObjectRecord>);
struct ActivationDependency {
    waiters: usize,
    target: Weak<ObjectRecord>,
}

pub(crate) struct HostInner {
    pub id: u64,
    pub name: String,
    pub principal: Value,
    pub principal_key: Rc<dyn Fn(&Value) -> String>,
    pub driver: Rc<dyn Driver>,
    services: RefCell<HashMap<String, Service>>,
    pub(crate) epochs: Rc<crate::epoch::EpochAuthority>,
    reload_control: RefCell<Option<Rc<reload::ReloadControl>>>,
    retirements: RefCell<Vec<CancellationToken>>,
    reload_ready: Cell<bool>,
    reload_bindings: RefCell<Option<reload::ReadyBindings>>,
    reload_ready_changed: RefCell<CancellationToken>,
    reload_gating: RefCell<HashSet<String>>,
    retire_permission: Cell<u64>,
    pub objects: RefCell<HashMap<Address, Rc<ObjectRecord>>>,
    next_object: Cell<u64>,
    next_root: Cell<u64>,
    pub in_factory: Cell<usize>,
    pub(crate) factory_pass: RefCell<Weak<crate::object::FactoryScope>>,
    pub publication_queue: Rc<PublicationQueue>,
    activation_waits: RefCell<HashMap<(u64, u64), ActivationDependency>>,
    pub ready_timeout: std::time::Duration,
    closing: Cell<bool>,
    closed: Cell<bool>,
    close_operation: RefCell<Option<futures::future::Shared<LocalBoxFuture<'static, Result<()>>>>>,
    pub errors: RefCell<Vec<RuntimeError>>,
    pub(crate) roots: Rc<crate::rpc::roots::RootRegistry>,
    rpc: rpc::RpcState,
    facets: facets::FacetState,
    pub(crate) registries: Rc<crate::registry::RegistryStore>,
    pub(crate) rpc_limits: HostRpcLimits,
}

pub(crate) fn contract_error(error: tembra_contracts::ContractError) -> RuntimeError {
    RuntimeError::invalid(error.reason, error.message)
}

impl Host {
    pub fn new(options: HostOptions, driver: Rc<dyn Driver>) -> Self {
        let id = NEXT_HOST.with(|next| {
            let id = next.get();
            next.set(id + 1);
            id
        });
        let epochs = crate::epoch::EpochAuthority::new();
        Self {
            inner: Rc::new_cyclic(|weak| HostInner {
                id,
                name: options.name,
                principal: options.self_principal,
                principal_key: options.principal_key,
                driver,
                services: RefCell::new(HashMap::new()),
                epochs: epochs.clone(),
                objects: RefCell::new(HashMap::new()),
                next_object: Cell::new(0),
                next_root: Cell::new(0),
                in_factory: Cell::new(0),
                factory_pass: RefCell::default(),
                publication_queue: PublicationQueue::new(),
                activation_waits: RefCell::new(HashMap::new()),
                ready_timeout: options.ready_timeout,
                closing: Cell::new(false),
                closed: Cell::new(false),
                close_operation: RefCell::new(None),
                reload_control: RefCell::new(None),
                retirements: RefCell::new(Vec::new()),
                reload_ready: Cell::new(false),
                reload_bindings: RefCell::new(None),
                reload_ready_changed: RefCell::new(CancellationToken::new()),
                reload_gating: RefCell::default(),
                retire_permission: Cell::new(0),
                errors: RefCell::new(Vec::new()),
                roots: crate::rpc::roots::RootRegistry::new(weak.clone()),
                rpc: rpc::RpcState::new(options.consumer_linger),
                facets: facets::FacetState::default(),
                registries: crate::registry::RegistryStore::new(weak.clone(), epochs.current()),
                rpc_limits: options.rpc_limits,
            }),
        }
    }

    pub fn rpc_limits(&self) -> &HostRpcLimits {
        &self.inner.rpc_limits
    }

    pub fn id(&self) -> u64 {
        self.inner.id
    }
    /// Observes the committed execution version without reserving a root or changing the Host.
    /// A later reload can advance this value; it is not an ownership lease.
    pub fn code_version(&self) -> u64 {
        self.inner.epochs.current().version.get()
    }
    pub fn name(&self) -> &str {
        &self.inner.name
    }
    /// Returns the retained background failure log without clearing it.
    ///
    /// An unsuccessful eager startup is reported once, including expected cancellation
    /// during Host close. Explicit retry preserves that original startup failure. Observer
    /// and cleanup failures are separate entries. This log is independent of [`Host::close`],
    /// whose returned error describes cleanup failures rather than startup cancellation.
    pub fn errors(&self) -> Vec<RuntimeError> {
        self.inner.errors.borrow().clone()
    }

    /// Registers one provider factory for a contract name.
    ///
    /// Factories declare resources synchronously; readiness and resource opening are driven
    /// separately. A host-lifetime provider schedules an initial pinned object.
    ///
    /// # Errors
    ///
    /// Refuses closing hosts, duplicate names, incompatible placement and invalid host
    /// lifetime options before installing the provider. Factory/resource errors are reported
    /// by readiness instead; factory callbacks can have external effects without rollback.
    /// A Ready barrier returns retryable [`ErrorCode::UpgradeConflict`] before installation.
    pub fn serve(
        &self,
        contract: Rc<Contract>,
        factory: Factory,
        options: ServeOptions,
    ) -> Result<()> {
        let batch = self.inner.begin_declarations()?;
        let result = self
            .inner
            .insert_service(contract, factory, options, None)
            .and_then(|id| self.inner.start_host_service(id).map(|_| ()));
        batch.finish();
        result
    }

    /// Acquires an independent claim immediately, scheduling activation when needed.
    ///
    /// # Errors
    ///
    /// Refuses closing hosts, unknown providers, incompatible contracts, invalid input or
    /// placement before scheduling a factory. An explicit acquisition can retry a failed
    /// host singleton and the new object retains its Host lifetime pin. That does not reset
    /// the original eager-startup result selected by [`Host::ready`]. Success may still be
    /// loading; use [`Reference::ready`] for activation errors. Dropping all clones releases
    /// this claim. A local Ready barrier or object gate can return retryable
    /// [`ErrorCode::UpgradeConflict`] when a new activation or unchecked consumer profile
    /// cannot be admitted.
    pub fn hold(
        &self,
        contract: Rc<Contract>,
        input: Value,
        options: GetOptions,
    ) -> Result<Reference> {
        self.inner.hold(contract, input, options)
    }

    /// Acquires a claim and waits for its readiness.
    ///
    /// # Errors
    ///
    /// Includes [`Self::hold`] and [`Reference::ready`] failures. Failure or dropping this
    /// future releases its new claim; other acquisitions and completed external effects
    /// remain independent.
    pub async fn get(
        &self,
        contract: Rc<Contract>,
        input: Value,
        options: GetOptions,
    ) -> Result<Reference> {
        let reference = self.hold(contract, input, options)?;
        reference.ready().await?;
        Ok(reference)
    }

    /// Runs a root scope using the host principal.
    ///
    /// The scope expires when this future completes or is dropped, including all descendant
    /// scopes and context-owned claims. Retaining a context clone does not extend it.
    /// Cancellation is cooperative once the callback runs.
    ///
    /// # Errors
    /// Returns `unavailable/closed` for a closed host, or the callback error unchanged.
    pub async fn root<T>(
        &self,
        callback: impl FnOnce(Context) -> LocalBoxFuture<'static, Result<T>>,
    ) -> Result<T> {
        self.root_as(self.inner.principal.clone(), callback).await
    }

    /// Runs [`Self::root`] with an explicit principal and the same scope guarantees.
    ///
    /// # Errors
    /// Returns `unavailable/closed` for a closed host, or the callback error unchanged.
    pub async fn root_as<T>(
        &self,
        principal: Value,
        callback: impl FnOnce(Context) -> LocalBoxFuture<'static, Result<T>>,
    ) -> Result<T> {
        if self.inner.closed.get() {
            return Err(RuntimeError::unavailable("closed", "host is closed"));
        }
        self.inner.epochs.check()?;
        let (ctx, _scope) = self.inner.new_root(principal);
        callback(ctx).await
    }

    /// Captures installed eager startup records now and waits for their readiness.
    ///
    /// The owned future excludes later declarations. Failed startup results remain selected
    /// after their object is disposed, including when an explicit acquisition successfully
    /// retries that singleton. Retry does not replace the original startup result. Dropping
    /// this waiter does not cancel installed work.
    ///
    /// # Errors
    /// Returns the first observed selected activation error without waiting for an unrelated
    /// pending startup. Referenced services are excluded; other activations are not undone.
    ///
    /// ```
    /// use std::{collections::BTreeMap, rc::Rc};
    /// use serde_json::json;
    /// use tembra_core::*;
    /// let driver = ManualDriver::new();
    /// let host = Host::new(HostOptions::new("ready", json!(null)), driver.clone());
    /// let before = host.ready();
    /// let cause = RuntimeError::new(ErrorCode::Forbidden, "policy", "refused");
    /// let observed = cause.clone();
    /// host.serve(Rc::new(contracts::Contract::local("later", 1, json!({}), BTreeMap::new())?),
    ///     Rc::new(move |_, _, _| Err(observed.clone())),
    ///     ServeOptions { lifetime: Lifetime::Host, ..Default::default() })?;
    /// driver.run_until(before)?; // Selection preceded the failing declaration.
    /// assert_eq!(driver.run_until(host.ready()).unwrap_err(), cause);
    /// assert_eq!(driver.run_until(host.ready()).unwrap_err(), cause);
    /// assert_eq!(host.errors(), vec![cause]); // Background failure is retained once.
    /// driver.run_until(host.close())?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn ready(&self) -> LocalBoxFuture<'static, Result<()>> {
        self.inner.ready_snapshot()
    }

    /// Closes every object, waiting for active invocations and resource cleanup to settle.
    ///
    /// # Guarantees
    ///
    /// Once first polled, the driver owns cleanup even if this waiter is dropped. Repeated
    /// calls await the same operation and return the same result. New acquisitions are
    /// refused immediately; cleanup may still call already-held dependencies with existing
    /// remote attachments. Pending disconnected remote dependencies fail with
    /// `unavailable/cleanup_dependency_disconnected` and the target name in their details;
    /// closing cannot admit the replacement attachment their readiness would require.
    /// Already terminal dependencies preserve their original failure reason.
    /// Cancellation signals do not prove that an invocation has terminated. A method or
    /// resource callback that never settles can therefore keep closure pending.
    ///
    /// # Errors
    ///
    /// Returns a structured `unavailable/cleanup` aggregate after attempting all cleanup
    /// callbacks. External effects and successful cleanup steps are not rolled back.
    pub async fn close(&self) -> Result<()> {
        let existing = self.inner.close_operation.borrow().clone();
        let operation = if let Some(operation) = existing {
            operation
        } else {
            self.inner.closing.set(true);
            self.inner.facets.close_runs();
            self.inner.flush_declarations();
            self.inner.rpc_begin_close();
            let reload = self.inner.reload_control.borrow().clone();
            if let Some(reload) = &reload {
                reload.cancellation.cancel(RuntimeError::aborted(
                    "host_closed",
                    "Host closed during reload",
                ));
            }
            let host = self.inner.clone();
            let operation = async move {
                // Notify dependency observers only after close_operation has been published,
                // so reentrant close calls still observe this same driver-owned operation.
                host.rpc_fail_disconnected_cleanup();
                host.rpc_reload_stopped().await;
                if let Some(reload) = reload {
                    reload.settled.cancelled().await;
                }
                let objects: Vec<_> = host.objects.borrow().values().cloned().collect();
                let cause = RuntimeError::new(ErrorCode::Gone, "host_closed", "Host closed");
                for object in &objects {
                    let activities = object
                        .installed
                        .borrow()
                        .values()
                        .filter_map(|installed| {
                            installed.declarations.activity_runs.borrow().clone()
                        })
                        .collect::<Vec<_>>();
                    for run in activities {
                        run.stop(cause.clone()).start();
                    }
                    let tokens = object
                        .active_contexts
                        .borrow()
                        .values()
                        .map(|entry| entry.cancellation.clone())
                        .collect::<Vec<_>>();
                    for token in tokens {
                        token.cancel(cause.clone());
                    }
                }
                let retirements = std::mem::take(&mut *host.retirements.borrow_mut());
                for retired in retirements {
                    retired.cancelled().await;
                }
                let result = host
                    .dispose_bulk(
                        objects,
                        RuntimeError::new(ErrorCode::Gone, "host_closed", "host closed"),
                    )
                    .await;
                host.registries.close();
                host.rpc_close_links();
                // Drop user-owned factory captures without holding the registry borrow.
                let services = std::mem::take(&mut *host.services.borrow_mut());
                drop(services);
                host.facets.release_runs();
                host.closed.set(true);
                result
            }
            .boxed_local()
            .shared();
            *self.inner.close_operation.borrow_mut() = Some(operation.clone());
            let owned = operation.clone();
            self.inner.driver.spawn(
                async move {
                    let _ = owned.await;
                }
                .boxed_local(),
            );
            operation
        };
        operation.await
    }

    /// Revokes matching principals except the host's own principal.
    ///
    /// The predicate runs synchronously without a registry borrow. Once polled through
    /// selection, driver-owned cleanup continues when this waiter is dropped. Cancellation
    /// is cooperative: cleanup waits for active invocations to settle physically.
    ///
    /// # Errors
    ///
    /// Cleanup failures are returned as a structured aggregate and retained by [`Self::errors`].
    /// Already performed callbacks and external effects are not rolled back. During Ready,
    /// returns retryable [`ErrorCode::UpgradeConflict`] before evaluating the predicate.
    pub async fn invalidate_principal(&self, predicate: impl Fn(&Value) -> bool) -> Result<()> {
        if self.inner.reload_ready.get() {
            return Err(RuntimeError::new(
                ErrorCode::UpgradeConflict,
                "upgrade_conflict",
                "reload commit barrier blocks principal disposal",
            ));
        }
        let own_key = (self.inner.principal_key)(&self.inner.principal);
        let snapshot: Vec<_> = self.inner.objects.borrow().values().cloned().collect();
        let objects = snapshot
            .into_iter()
            .filter(|object| {
                (self.inner.principal_key)(&object.principal) != own_key
                    && predicate(&object.principal)
            })
            .collect();
        let host = self.inner.clone();
        let operation = async move {
            let result = host
                .dispose_bulk(
                    objects,
                    RuntimeError::new(
                        ErrorCode::Gone,
                        "principal_invalidated",
                        "principal invalidated",
                    ),
                )
                .await;
            if let Err(error) = &result {
                host.errors.borrow_mut().push(error.clone());
            }
            result
        }
        .boxed_local()
        .shared();
        let owned = operation.clone();
        self.inner.driver.spawn(
            async move {
                let _ = owned.await;
            }
            .boxed_local(),
        );
        operation.await
    }
}

impl Context {
    /// Acquires a claim owned by this scope, inheriting its principal unless overridden.
    ///
    /// # Errors
    /// Refuses foreign, expired or cancelled contexts and propagates acquisition/readiness
    /// failures. Cancellation or scope expiry wakes a pending wait and releases this claim.
    /// A returned reference's clones share that claim, which scope settlement also releases.
    pub async fn get(
        &self,
        contract: Rc<Contract>,
        input: Value,
        mut options: GetOptions,
    ) -> Result<Reference> {
        let host = self
            .attachment::<HostAttachment>()
            .and_then(|attachment| attachment.0.upgrade())
            .ok_or_else(|| RuntimeError::invalid("foreign_ctx", "context has no host"))?;
        self.check(host.id)?;
        if options.principal.is_none() {
            options.principal = Some(self.principal().clone());
        }
        let reference = host.hold(contract, input, options)?;
        let _activation_wait = reference
            .claim
            .local_object()
            .map(|object| host.initializer_wait(self, object))
            .transpose()?
            .flatten();
        let owned = reference.clone();
        self.on_settle(move || owned.release())?;
        let cancelled =
            futures::future::select(self.cancellation().cancelled(), self.scope_ended())
                .map(|either| match either {
                    futures::future::Either::Left((error, _))
                    | futures::future::Either::Right((error, _)) => error,
                })
                .boxed_local();
        let result = match futures::future::select(reference.ready().boxed_local(), cancelled).await
        {
            futures::future::Either::Left((result, _)) => result,
            futures::future::Either::Right((error, _)) => {
                reference.release();
                return Err(error);
            }
        };
        if let Err(error) = self.check(host.id).and(result) {
            reference.release();
            return Err(error);
        }
        Ok(reference)
    }
}

impl HostInner {
    pub(crate) fn is_closed(&self) -> bool {
        self.closed.get()
    }
    pub(crate) fn is_closing(&self) -> bool {
        self.closing.get()
    }

    pub fn new_root(self: &Rc<Self>, principal: Value) -> (Context, crate::ScopeGuard) {
        let root_id = self.next_root_id();
        let epoch = self.epochs.current();
        let (ctx, scope) = Context::new_root(self.id, root_id, epoch.version.get(), principal);
        let ctx = ctx.with_cancellation(epoch.cancellation.child());
        ctx.set_attachment(Rc::new(HostAttachment(Rc::downgrade(self))));
        self.roots.register_origin(&ctx);
        (ctx, scope)
    }

    pub(crate) fn activity_root(
        self: &Rc<Self>,
        object: &Rc<ObjectRecord>,
        admission: &crate::activity::ActivityAdmission,
    ) -> Result<(Context, crate::ScopeGuard)> {
        self.epochs.check()?;
        admission.check()?;
        if self.closing.get() || object.phase.get() != Phase::Live {
            return Err(RuntimeError::unavailable(
                "activity_owner",
                "activity owner is unavailable",
            ));
        }
        let (context, guard) = self.new_root(object.principal.clone());
        admission.signal().link_child(context.cancellation());
        context.check(self.id)?;
        Ok((context, guard))
    }

    pub(crate) fn enter_listener(
        self: &Rc<Self>,
        object: &Rc<ObjectRecord>,
        context: &Context,
    ) -> Result<(Context, crate::invocation::LogicalInvocationGuard)> {
        context.check(self.id)?;
        let installed = object
            .installed
            .borrow()
            .get(&context.code_version())
            .cloned()
            .ok_or_else(crate::epoch::superseded)?;
        crate::invocation::enter(
            self,
            object.clone(),
            installed.implementation.clone(),
            context,
        )
    }

    pub fn hold(
        self: &Rc<Self>,
        contract: Rc<Contract>,
        input: Value,
        options: GetOptions,
    ) -> Result<Reference> {
        self.epochs.check()?;
        if self.closing.get() {
            return Err(RuntimeError::unavailable("closed", "host is closing"));
        }
        let input = contract.normalize_input(&input).map_err(contract_error)?;
        if contract.kind() == ContractKind::Remote
            && contract
                .resolve_host(&input)
                .map_err(contract_error)?
                .as_deref()
                != Some(self.name.as_str())
        {
            return self.rpc_hold_remote(contract, input, options.state, None, None);
        }
        let service = self
            .services
            .borrow()
            .get(contract.name())
            .cloned()
            .ok_or_else(|| RuntimeError::unavailable("unknown_type", "type is not served"))?;
        check_binding(&service.contract, &contract, &service.options)?;
        let input = contract.normalize_input(&input).map_err(contract_error)?;
        if contract
            .resolve_host(&input)
            .map_err(contract_error)?
            .is_some_and(|name| name != self.name)
        {
            return Err(RuntimeError::invalid(
                "placement",
                "input resolves to another host",
            ));
        }
        let provider_input = service
            .contract
            .normalize_input(&input)
            .map_err(contract_error)?;
        if contract.identity(&input).map_err(contract_error)?
            != service
                .contract
                .identity(&provider_input)
                .map_err(contract_error)?
        {
            return Err(RuntimeError::new(
                ErrorCode::ContractMismatch,
                "identity",
                "consumer and provider normalize to different object identities",
            ));
        }
        let principal = if service.options.lifetime == Lifetime::Host {
            if contract.identity(&input).map_err(contract_error)?
                != contract
                    .identity(&serde_json::json!({}))
                    .map_err(contract_error)?
            {
                return Err(RuntimeError::invalid(
                    "input",
                    "host object input must identify its singleton",
                ));
            }
            self.principal.clone()
        } else {
            options.principal.unwrap_or_else(|| self.principal.clone())
        };
        let key = Address(
            contract.name().into(),
            contract.identity(&input).map_err(contract_error)?,
            (self.principal_key)(&principal),
        );
        let existing = self.objects.borrow().get(&key).cloned();
        if let Some(object) = &existing
            && matches!(object.phase.get(), Phase::Activating | Phase::Live)
            && !object.cancellation.is_cancelled()
        {
            self.check_reload_binding(object.id, contract.fingerprint())?;
            object
                .linger_generation
                .set(object.linger_generation.get() + 1);
            let reference = Reference::new(self, object.clone(), contract, options.state);
            self.flush_declarations();
            return Ok(reference);
        }
        if self.reload_ready.get() || self.reload_gating.borrow().contains(contract.name()) {
            return Err(RuntimeError::new(
                ErrorCode::UpgradeConflict,
                "upgrade_conflict",
                "reload blocks new activation of this object until commit or abort",
            ));
        }
        let object = self.new_object(key.clone(), provider_input, principal, service);
        self.objects.borrow_mut().insert(key, object.clone());
        let reference = Reference::new(self, object.clone(), contract, options.state);
        self.schedule_activation(object, existing);

        Ok(reference)
    }

    fn new_object(
        self: &Rc<Self>,
        key: Address,
        provider_input: Value,
        principal: Value,
        service: Service,
    ) -> Rc<ObjectRecord> {
        let id = self.next_object.get() + 1;
        self.next_object.set(id);
        let host_lifetime = service.options.lifetime == Lifetime::Host;
        let activation_service = Rc::new(service.clone());
        Rc::new_cyclic(|weak| ObjectRecord {
            id,
            activation_service: RefCell::new(Some(activation_service)),
            installed: RefCell::new(std::collections::BTreeMap::new()),
            current_epoch: Cell::new(self.epochs.current().version.get()),
            min_code_version: Cell::new(self.epochs.current().version.get()),
            gate: RefCell::new(None),
            activities: crate::activity::ActivityDeclarations::new(),
            service_ordinal: service.ordinal.0,
            startup: self.facets.startups.borrow().get(&service.ordinal).cloned(),
            registrations: RefCell::new(Vec::new()),
            key: key.clone(),
            input: provider_input,
            principal,
            contract: service.contract,
            lifetime: service.options.lifetime,
            linger: service.options.linger,
            factory: RefCell::new(Some(service.factory)),
            state: self.state_graph(weak.clone()),
            state_exposures: RefCell::new(Vec::new()),
            phase: Cell::new(Phase::Activating),
            error: RefCell::new(None),
            implementation: RefCell::new(None),
            adapter_views: RefCell::new(HashMap::new()),
            dependencies: RefCell::new(HashMap::new()),
            resources: RefCell::new(Vec::new()),
            opened: RefCell::new(Vec::new()),
            claims: RefCell::new(Vec::new()),
            references: Cell::new(0),
            invocations: Cell::new(0),
            next_invocation: Cell::new(0),
            active_contexts: RefCell::new(HashMap::new()),
            ready_waiters: RefCell::new(HashMap::new()),
            next_waiter: Cell::new(0),
            dispose_waiters: RefCell::new(Vec::new()),
            idle_waiters: RefCell::new(Vec::new()),
            cancellation: CancellationToken::new(),
            linger_generation: Cell::new(0),
            host_pin: Cell::new(host_lifetime),
        })
    }

    pub fn declare_get(
        self: &Rc<Self>,
        owner: &Rc<ObjectRecord>,
        contract: Rc<Contract>,
        input: Value,
        options: GetOptions,
    ) -> Result<Reference> {
        let reference = self.hold(contract, input, options)?;
        let key = reference.address();
        if let Some(existing) = owner.dependencies.borrow().get(&key).cloned() {
            reference.release();
            if existing.contract().fingerprint() != reference.contract().fingerprint() {
                return Err(RuntimeError::invalid(
                    "duplicate",
                    "one declaration cannot bind two contracts for the same object address",
                ));
            }
            return Ok(existing);
        }
        if reference
            .claim
            .local_object()
            .is_some_and(|object| self.reaches(object, owner.id, &mut HashSet::new()))
        {
            reference.release();
            return Err(RuntimeError::invalid("cycle", "object reference cycle"));
        }
        owner
            .dependencies
            .borrow_mut()
            .insert(key, reference.clone());
        reference.claim.object_owned.set(true);
        Ok(reference)
    }

    pub(crate) fn declare_reload_get(
        self: &Rc<Self>,
        owner: &Rc<ObjectRecord>,
        candidate: &Rc<reload::CandidateDeclarations>,
        contract: Rc<Contract>,
        input: Value,
        options: GetOptions,
    ) -> Result<Reference> {
        let reference = candidate.catalog.hold(contract, input, options)?;
        let key = reference.address();
        if let Some(existing) = candidate.dependencies.borrow().get(&key).cloned() {
            reference.release();
            if existing.contract().fingerprint() != reference.contract().fingerprint() {
                return Err(RuntimeError::invalid(
                    "duplicate",
                    "candidate address has two consumer contracts",
                ));
            }
            return Ok(existing);
        }
        if reference.claim.local_object().is_some_and(|object| {
            candidate
                .catalog
                .reaches(object, owner.id, &mut HashSet::new())
        }) {
            reference.release();
            return Err(RuntimeError::invalid("cycle", "object reference cycle"));
        }
        let installed = owner
            .installed
            .borrow()
            .get(&owner.current_epoch.get())
            .cloned();
        if let Some(existing) = installed
            .as_ref()
            .and_then(|installed| installed.declarations.dependencies.get(&key))
            && existing.same_dependency(&reference)
        {
            reference.release();
            candidate
                .dependencies
                .borrow_mut()
                .insert(key, existing.clone());
            return Ok(existing.clone());
        }
        reference.claim.object_owned.set(true);
        candidate
            .dependencies
            .borrow_mut()
            .insert(key, reference.clone());
        Ok(reference)
    }

    fn reaches(&self, object: &ObjectRecord, target: u64, visited: &mut HashSet<u64>) -> bool {
        if object.id == target {
            return true;
        }
        if !visited.insert(object.id) {
            return false;
        }
        let mut dependencies: Vec<_> = object
            .dependencies
            .borrow()
            .values()
            .filter_map(|reference| reference.claim.local_object().cloned())
            .collect();
        dependencies.extend(
            self.activation_waits
                .borrow()
                .iter()
                .filter(|((from, _), _)| *from == object.id)
                .filter_map(|(_, edge)| edge.target.upgrade()),
        );
        dependencies
            .iter()
            .any(|dependency| self.reaches(dependency, target, visited))
    }

    pub(crate) fn initializer_wait(
        self: &Rc<Self>,
        ctx: &Context,
        target: &Rc<ObjectRecord>,
    ) -> Result<Option<ActivationWait>> {
        let Some(owner) = ctx
            .attachment::<InitializingObject>()
            .and_then(|owner| owner.0.upgrade())
        else {
            return Ok(None);
        };
        if target.phase.get() == Phase::Live {
            return Ok(None);
        }
        if self.reaches(target, owner.id, &mut HashSet::new()) {
            return Err(RuntimeError::invalid(
                "cycle",
                "initializer cannot await its own or dependent activation",
            ));
        }
        let key = (owner.id, target.id);
        self.activation_waits
            .borrow_mut()
            .entry(key)
            .and_modify(|edge| edge.waiters += 1)
            .or_insert_with(|| ActivationDependency {
                waiters: 1,
                target: Rc::downgrade(target),
            });
        Ok(Some(ActivationWait {
            host: Rc::downgrade(self),
            key,
        }))
    }

    fn state_graph(self: &Rc<Self>, object: Weak<ObjectRecord>) -> Rc<StateGraph> {
        let read_object = object.clone();
        let read = Rc::new(move || {
            let object = read_object.upgrade().ok_or_else(|| {
                RuntimeError::new(ErrorCode::Gone, "state", "state owner no longer exists")
            })?;
            if matches!(object.phase.get(), Phase::Disposed | Phase::Failed) {
                return Err(RuntimeError::new(
                    ErrorCode::Gone,
                    "state",
                    "state owner is disposed",
                ));
            }
            Ok(())
        });
        let subscribe_object = object.clone();
        let subscribe_host = Rc::downgrade(self);
        let subscribe = Rc::new(move || {
            let host = subscribe_host
                .upgrade()
                .ok_or_else(|| RuntimeError::unavailable("host", "host no longer exists"))?;
            if host.in_factory.get() > 0 {
                return Err(RuntimeError::invalid(
                    "read_only",
                    "state subscriptions cannot start inside a factory",
                ));
            }
            let object = subscribe_object.upgrade().ok_or_else(|| {
                RuntimeError::new(ErrorCode::Gone, "state", "state owner no longer exists")
            })?;
            if !matches!(object.phase.get(), Phase::Activating | Phase::Live) {
                return Err(RuntimeError::new(
                    ErrorCode::Gone,
                    "state",
                    "state owner is disposing",
                ));
            }
            Ok(())
        });
        let write_host = Rc::downgrade(self);
        let write = Rc::new(move |ctx: &Context| {
            let host = write_host
                .upgrade()
                .ok_or_else(|| RuntimeError::unavailable("host", "host no longer exists"))?;
            ctx.check(host.id)?;
            if host.in_factory.get() > 0 {
                return Err(RuntimeError::invalid(
                    "read_only",
                    "state cannot be written inside a factory",
                ));
            }
            let object = object.upgrade().ok_or_else(|| {
                RuntimeError::new(ErrorCode::Gone, "state", "state owner no longer exists")
            })?;
            if !matches!(object.phase.get(), Phase::Activating | Phase::Live) {
                return Err(RuntimeError::new(
                    ErrorCode::Gone,
                    "state",
                    "state owner is disposing",
                ));
            }
            host.roots.touch(ctx, &object)?;
            object.cancellation.check()
        });
        let reporting_host = Rc::downgrade(self);
        let report = Rc::new(move |error| {
            if let Some(host) = reporting_host.upgrade() {
                host.errors.borrow_mut().push(error);
            }
        });
        StateGraph::new(
            read,
            subscribe,
            write,
            report,
            self.publication_queue.clone(),
        )
    }

    async fn activate(self: &Rc<Self>, object: Rc<ObjectRecord>) {
        let result = async {
            self.prepare_activation(&object).await?;
            while self.reload_ready.get() {
                let changed = self.reload_ready_changed.borrow().clone();
                if !self.reload_ready.get() {
                    break;
                }
                match futures::future::select(changed.cancelled(), object.cancellation.cancelled())
                    .await
                {
                    futures::future::Either::Left(_) => {}
                    futures::future::Either::Right((cause, _)) => return Err(cause),
                }
            }
            self.epochs.check()?;
            if self.closing.get() {
                return Err(RuntimeError::new(
                    ErrorCode::Gone,
                    "host_closed",
                    "Host closed before activation publication",
                ));
            }
            let current = self.epochs.current().version.get();
            if current != object.current_epoch.get() {
                let original = object
                    .activation_service
                    .borrow()
                    .clone()
                    .expect("activation service retained");
                let selected = self.services.borrow().get(&object.key.0).cloned();
                if !selected.is_some_and(|selected| {
                    selected.ordinal == original.ordinal
                        && selected.owner == original.owner
                        && Rc::ptr_eq(&selected.factory, &original.factory)
                }) {
                    return Err(RuntimeError::new(
                        ErrorCode::UpgradeConflict,
                        "upgrade_conflict",
                        "activation service changed before publication",
                    ));
                }
                // The initializer already completed in its real original root. Only the
                // subsequently published implementation receives current-epoch authority.
                object.current_epoch.set(current);
                object.min_code_version.set(current);
            }
            self.finalize_activation(&object)
        }
        .await;
        match result {
            Ok(()) => {
                // Finalization already adopted tables, state and Live before notifications.
            }
            result => {
                let error = result
                    .err()
                    .unwrap_or_else(|| RuntimeError::aborted("abandoned", "activation abandoned"));
                *object.error.borrow_mut() = Some(error.clone());
                object.phase.set(Phase::Failed);
                if let Some(startup) = &object.startup {
                    self.report_startup(startup, Err(error.clone()));
                }
                object.finish_ready(Err(error));
                if let Err(cleanup) = self.dispose(object, false, None).await {
                    self.errors.borrow_mut().push(cleanup);
                }
            }
        }
    }

    async fn prepare_activation(self: &Rc<Self>, object: &Rc<ObjectRecord>) -> Result<()> {
        object.cancellation.check()?;
        // The service retains retry authority. An object needs its factory for this pass only;
        // retained address handles must not retain its callback after managed cleanup.
        let factory = object
            .factory
            .borrow_mut()
            .take()
            .expect("object activation consumes its factory once");
        let implementation = {
            let pass = Rc::new(crate::object::FactoryScope {
                target: crate::host::reload::DeclarationTarget::Activation,
            });
            let previous = self.factory_pass.replace(Rc::downgrade(&pass));
            self.in_factory.set(self.in_factory.get() + 1);
            let _guard = FactoryPass {
                host: self,
                _pass: pass.clone(),
                previous,
            };
            let mut declaration = Declaration {
                host: self.clone(),
                object: object.clone(),
                pass: Rc::downgrade(&pass),
            };
            factory(
                object.input.clone(),
                object.principal.clone(),
                &mut declaration,
            )
        };
        drop(factory);
        let mut implementation = implementation?;
        Self::validate_implementation(&object.contract, &implementation)?;
        let variant = implementation.variant.as_deref().unwrap_or("");
        let members = object
            .contract
            .members(variant)
            .expect("validated implementation variant");
        let mut exposures = Vec::new();
        for (name, source) in &mut implementation.states {
            let Member::State { schema } = &members[name] else {
                unreachable!()
            };
            if !object.state.owns(source) {
                *source = object.state.forward(source.clone())?;
            }
            exposures.push(object.state.expose(source, schema.clone())?);
        }
        *object.state_exposures.borrow_mut() = exposures;
        *object.implementation.borrow_mut() = Some(Rc::new(implementation));
        let resources = object.resources.borrow().clone();
        let references: Vec<_> = object.dependencies.borrow().values().cloned().collect();
        let resource_opens = resources
            .into_iter()
            .map(|resource| self.open_resource(object.clone(), resource));
        let dependencies = async move {
            let results = futures::future::join_all(references.iter().map(Reference::ready)).await;
            results.into_iter().collect::<Result<Vec<_>>>().map(|_| ())
        };
        let (opens, deps) = futures::join!(futures::future::join_all(resource_opens), dependencies);
        let resources = object.resources.borrow().clone();
        for resource in resources {
            let opening = resource.opening.borrow_mut().take();
            drop(opening);
        }
        for result in opens {
            result?;
        }
        deps?;
        object.cancellation.check()?;
        let hydration = object.state.hydrate_external();
        let stopped = futures::future::select(
            object.cancellation.cancelled(),
            self.driver.sleep(self.ready_timeout),
        )
        .map(|result| match result {
            futures::future::Either::Left((error, _)) => error,
            futures::future::Either::Right(_) => RuntimeError::unavailable(
                "ready_timeout",
                "external state hydration deadline elapsed",
            ),
        })
        .boxed_local();
        match futures::future::select(hydration, stopped).await {
            futures::future::Either::Left((result, _)) => result?,
            futures::future::Either::Right((error, _)) => return Err(error),
        }
        let initializers = object.state.take_initializers();
        let cancellation = object.cancellation.child();
        let failure = Rc::new(RefCell::new(None));
        futures::future::join_all(initializers.into_iter().map(|plan| {
            let host = self.clone();
            let object = object.clone();
            let cancellation = cancellation.clone();
            let failure = failure.clone();
            async move {
                let result = async {
                    let (ctx, _scope) = host.new_root(host.principal.clone());
                    let ctx = ctx.with_cancellation(cancellation.child());
                    ctx.set_attachment(Rc::new(InitializingObject(Rc::downgrade(&object))));
                    ctx.check(host.id)?;
                    let value = (plan.initialize)(ctx).await?;
                    cancellation.check()?;
                    object.state.initialize(vec![(plan.cell, value)])
                }
                .await;
                if let Err(error) = result {
                    let first = {
                        let mut failure = failure.borrow_mut();
                        if failure.is_none() {
                            *failure = Some(error.clone());
                            true
                        } else {
                            false
                        }
                    };
                    if first {
                        cancellation.cancel(error);
                    }
                }
            }
        }))
        .await;
        if let Some(error) = failure.borrow_mut().take() {
            return Err(error);
        }
        object.cancellation.check()?;
        Ok(())
    }

    pub(crate) fn validate_implementation(
        contract: &Contract,
        implementation: &Implementation,
    ) -> Result<()> {
        Self::validate_view(contract, implementation, true)
    }

    pub(crate) fn validate_view(
        contract: &Contract,
        implementation: &Implementation,
        strict: bool,
    ) -> Result<()> {
        let variant = match contract.kind() {
            ContractKind::Local => {
                if implementation
                    .variant
                    .as_deref()
                    .is_some_and(|variant| !variant.is_empty())
                {
                    return Err(RuntimeError::invalid(
                        "variant",
                        "local implementations cannot select a named variant",
                    ));
                }
                ""
            }
            ContractKind::Remote => implementation.variant.as_deref().ok_or_else(|| {
                RuntimeError::invalid("variant", "remote implementation must select a variant")
            })?,
        };
        let members = contract.members(variant).ok_or_else(|| {
            RuntimeError::new(
                ErrorCode::ContractMismatch,
                "variant",
                "implementation variant is absent from the consumer contract",
            )
        })?;
        for name in implementation
            .methods
            .keys()
            .chain(implementation.json_methods.keys())
        {
            if strict && !matches!(members.get(name), Some(Member::Method { .. })) {
                return Err(RuntimeError::invalid(
                    "implementation",
                    format!("undeclared method {name}"),
                ));
            }
        }
        for name in implementation.states.keys() {
            if strict && !matches!(members.get(name), Some(Member::State { .. })) {
                return Err(RuntimeError::invalid(
                    "implementation",
                    "undeclared state member",
                ));
            }
        }
        for name in implementation.opaque.keys() {
            if strict && !matches!(members.get(name), Some(Member::Opaque { .. })) {
                return Err(RuntimeError::invalid(
                    "implementation",
                    "undeclared opaque member",
                ));
            }
        }
        if contract.kind() == ContractKind::Remote && !implementation.methods.is_empty() {
            return Err(RuntimeError::invalid(
                "implementation",
                "remote methods require a validated portable registration",
            ));
        }
        for (name, member) in members {
            if let Member::Method { args: None, .. } = member
                && implementation
                    .methods
                    .get(name)
                    .is_some_and(|method| method.args != std::any::TypeId::of::<()>())
            {
                return Err(RuntimeError::invalid(
                    "implementation",
                    "no-argument method handler must accept unit",
                ));
            }
            if matches!(member, Member::Method { .. })
                && !implementation.methods.contains_key(name)
                && !implementation.json_methods.contains_key(name)
            {
                return Err(RuntimeError::invalid(
                    "implementation",
                    format!("missing method {name}"),
                ));
            }
            if matches!(member, Member::Opaque { .. }) && !implementation.opaque.contains_key(name)
            {
                return Err(RuntimeError::invalid(
                    "implementation",
                    "missing opaque member",
                ));
            }
            if matches!(member, Member::State { .. }) && !implementation.states.contains_key(name) {
                return Err(RuntimeError::invalid(
                    "implementation",
                    "state/opaque members need a matching implementation",
                ));
            }
        }
        Ok(())
    }

    fn open_resource(
        self: &Rc<Self>,
        object: Rc<ObjectRecord>,
        resource: Rc<ResourceRecord>,
    ) -> futures::future::Shared<LocalBoxFuture<'static, Result<()>>> {
        if let Some(opening) = resource.opening.borrow().clone() {
            return opening;
        }
        let host = self.clone();
        let selected = resource.clone();
        let future = async move {
            let spec = selected.spec();
            let dependencies: Vec<_> = spec
                .deps
                .iter()
                .map(|dependency| match dependency {
                    ResourceDependency::Reference(reference) => {
                        let reference = reference.clone();
                        async move { reference.ready().await }.boxed_local()
                    }
                    ResourceDependency::Resource(resource) => host
                        .open_resource(object.clone(), resource.0.clone())
                        .boxed_local(),
                })
                .collect();
            for result in futures::future::join_all(dependencies).await {
                result?;
            }
            object.cancellation.check()?;
            let value = (spec.open)(object.cancellation.clone()).await?;
            *selected.value.borrow_mut() = Some(value);
            object.opened.borrow_mut().push(selected.clone());
            if object.cancellation.is_cancelled() {
                selected.close().await?;
                return Err(RuntimeError::aborted(
                    "late_open",
                    "resource completed after cancellation",
                ));
            }
            Ok(())
        }
        .boxed_local()
        .shared();
        *resource.opening.borrow_mut() = Some(future.clone());
        future
    }

    pub fn idle(self: &Rc<Self>, object: Rc<ObjectRecord>) {
        if object.references.get() > 0 || object.host_pin.get() || object.invocations.get() > 0 {
            return;
        }
        match object.phase.get() {
            Phase::Activating => {
                object
                    .cancellation
                    .cancel(RuntimeError::aborted("abandoned", "activation abandoned"));
            }
            Phase::Live => {
                let generation = object.linger_generation.get() + 1;
                object.linger_generation.set(generation);
                let host = self.clone();
                self.driver.spawn(
                    async move {
                        if !object.linger.is_zero() {
                            host.driver.sleep(object.linger).await;
                        }
                        if object.linger_generation.get() != generation
                            || object.references.get() != 0
                            || object.invocations.get() != 0
                            || object.host_pin.get()
                        {
                            return;
                        }
                        if let Err(error) = host.dispose(object, false, None).await {
                            host.errors.borrow_mut().push(error);
                        }
                    }
                    .boxed_local(),
                );
            }
            _ => {}
        }
    }

    pub async fn dispose(
        self: &Rc<Self>,
        object: Rc<ObjectRecord>,
        force: bool,
        reason: Option<RuntimeError>,
    ) -> Result<()> {
        if object.phase.get() == Phase::Disposed {
            return Ok(());
        }
        if object.phase.get() == Phase::Disposing {
            object.disposed().await;
            return Ok(());
        }
        if object.phase.get() == Phase::Activating {
            object.host_pin.set(false);
            object.cancellation.cancel(
                reason.unwrap_or_else(|| RuntimeError::aborted("disposal", "object disposed")),
            );
            let _ = object.ready().await;
            object.disposed().await;
            return Ok(());
        }
        object.phase.set(Phase::Disposing);
        object.host_pin.set(false);
        let error = reason
            .unwrap_or_else(|| RuntimeError::new(ErrorCode::Gone, "disposed", "object disposed"));
        if force {
            *object.error.borrow_mut() = Some(error.clone());
        }
        let activities = object
            .installed
            .borrow()
            .values()
            .filter_map(|installed| installed.declarations.activity_runs.borrow().clone())
            .collect::<Vec<_>>();
        for run in activities {
            run.stop(error.clone()).start();
        }
        // Resource open observes activation ownership, which ends on every disposal.
        // Ordinary disposal still lets already admitted method Contexts settle normally.
        object.cancellation.cancel(error.clone());
        if force {
            let tokens: Vec<_> = object
                .active_contexts
                .borrow()
                .values()
                .map(|entry| entry.cancellation.clone())
                .collect();
            for token in tokens {
                token.cancel(error.clone());
            }
            let claims: Vec<_> = object
                .claims
                .borrow()
                .iter()
                .filter_map(Weak::upgrade)
                .collect();
            for claim in claims {
                claim.mark_gone(error.clone());
            }
        }
        while object.invocations.get() > 0 {
            let (send, receive) = futures::channel::oneshot::channel();
            object.idle_waiters.borrow_mut().push(send);
            let _ = receive.await;
        }
        let mut errors = Vec::new();
        match self.registries.remove_owner(object.id) {
            Ok(mut candidate) => match self.registries.preflight(&candidate) {
                Ok(()) => {
                    let hold = self.publication_queue.hold();
                    let publications = self.registries.adopt(&mut candidate);
                    self.publication_queue.append_registry_batch(publications);
                    candidate.finish();
                    drop(hold);
                }
                Err(error) => errors.push(error),
            },
            Err(error) => errors.push(error),
        }
        let versions = object
            .installed
            .borrow()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut opened = Vec::new();
        for version in &versions {
            for resource in version.declarations.resources.opened() {
                if !opened.iter().any(|other| Rc::ptr_eq(other, resource)) {
                    opened.push(resource.clone());
                }
            }
        }
        for resource in std::mem::take(&mut *object.opened.borrow_mut()) {
            if !opened.iter().any(|other| Rc::ptr_eq(other, &resource)) {
                opened.push(resource);
            }
        }
        for resource in opened.into_iter().rev() {
            if let Err(error) = resource.close().await {
                errors.push(error);
            }
        }
        object.state.terminate(error.clone());
        let exposures = std::mem::take(&mut *object.state_exposures.borrow_mut());
        drop(exposures);
        let refs = std::mem::take(&mut *object.dependencies.borrow_mut());
        for reference in refs.values() {
            reference.release();
        }
        for version in &versions {
            for reference in version.declarations.dependencies.values() {
                reference.release();
            }
            version.declarations.activities.clear().start();
        }
        drop(versions);
        let implementation = object.implementation.borrow_mut().take();
        let views = std::mem::take(&mut *object.adapter_views.borrow_mut());
        drop((implementation, views));
        let resources = std::mem::take(&mut *object.resources.borrow_mut());
        for resource in &resources {
            let opening = resource.opening.borrow_mut().take();
            drop(opening);
        }
        drop(resources);
        let factory = object.factory.borrow_mut().take();
        let installed = std::mem::take(&mut *object.installed.borrow_mut());
        let activation_service = object.activation_service.borrow_mut().take();
        drop((factory, installed, activation_service));
        object.activities.clear().start();
        object.phase.set(Phase::Disposed);
        object.notify_claims();
        let waiters = std::mem::take(&mut *object.dispose_waiters.borrow_mut());
        for waiter in waiters {
            let _ = waiter.send(());
        }
        if self
            .objects
            .borrow()
            .get(&object.key)
            .is_some_and(|stored| Rc::ptr_eq(stored, &object))
        {
            self.objects.borrow_mut().remove(&object.key);
        }
        aggregate(errors)
    }

    async fn dispose_bulk(
        self: &Rc<Self>,
        mut objects: Vec<Rc<ObjectRecord>>,
        reason: RuntimeError,
    ) -> Result<()> {
        for object in &objects {
            *object.error.borrow_mut() = Some(reason.clone());
            object.cancellation.cancel(reason.clone());
            let tokens: Vec<_> = object
                .active_contexts
                .borrow()
                .values()
                .map(|entry| entry.cancellation.clone())
                .collect();
            for token in tokens {
                token.cancel(reason.clone());
            }
            let claims: Vec<_> = object
                .claims
                .borrow()
                .iter()
                .filter_map(Weak::upgrade)
                .collect();
            for claim in claims {
                if !claim.object_owned.get() {
                    claim.mark_gone(reason.clone());
                }
            }
        }
        let mut errors = Vec::new();
        while !objects.is_empty() {
            let index = objects
                .iter()
                .position(|candidate| {
                    !objects.iter().any(|other| {
                        other.id != candidate.id
                            && other
                                .dependencies
                                .borrow()
                                .values()
                                .any(|reference| reference.object_id() == candidate.id)
                            || (other.id != candidate.id
                                && other.installed.borrow().values().any(|version| {
                                    version
                                        .declarations
                                        .dependencies
                                        .values()
                                        .any(|reference| reference.object_id() == candidate.id)
                                }))
                    })
                })
                .unwrap_or(0);
            let object = objects.remove(index);
            if let Err(error) = self.dispose(object, true, Some(reason.clone())).await {
                errors.push(error);
            }
        }
        aggregate(errors)
    }
}

fn aggregate(errors: Vec<RuntimeError>) -> Result<()> {
    if errors.is_empty() {
        return Ok(());
    }
    let mut error = RuntimeError::unavailable("cleanup", "one or more cleanup callbacks failed");
    error.set_details(serde_json::json!({ "errors": errors }));
    Err(error)
}

fn check_binding(provider: &Contract, consumer: &Contract, options: &ServeOptions) -> Result<()> {
    let mismatch = |error: tembra_contracts::ContractError| {
        RuntimeError::new(ErrorCode::ContractMismatch, error.reason, error.message)
    };
    if provider.name() != consumer.name()
        || provider.kind() != consumer.kind()
        || provider.placement() != consumer.placement()
    {
        return Err(RuntimeError::new(
            ErrorCode::ContractMismatch,
            "placement",
            "contract name, kind and placement must agree even through an adapter",
        ));
    }
    tembra_contracts::identity_compatible(provider.input(), consumer.input()).map_err(mismatch)?;
    if provider.version() == consumer.version() {
        provider.covers(consumer).map_err(mismatch)
    } else if options.accepts.contains_key(&consumer.version()) {
        tembra_contracts::input_compatible(consumer.input(), provider.input()).map_err(mismatch)
    } else {
        Err(RuntimeError::new(
            ErrorCode::ContractMismatch,
            "version",
            "provider has no explicit adapter for the consumer version",
        ))
    }
}

struct FactoryPass<'a> {
    host: &'a Rc<HostInner>,
    _pass: Rc<crate::object::FactoryScope>,
    previous: Weak<crate::object::FactoryScope>,
}
impl Drop for FactoryPass<'_> {
    fn drop(&mut self) {
        self.host
            .factory_pass
            .replace(std::mem::take(&mut self.previous));
        self.host
            .in_factory
            .set(self.host.in_factory.get().saturating_sub(1));
        if !std::thread::panicking() {
            self.host.flush_declarations();
        }
    }
}

pub(crate) struct ActivationWait {
    host: Weak<HostInner>,
    key: (u64, u64),
}

impl Drop for ActivationWait {
    fn drop(&mut self) {
        if let Some(host) = self.host.upgrade() {
            let mut waits = host.activation_waits.borrow_mut();
            if let Some(edge) = waits.get_mut(&self.key) {
                edge.waiters -= 1;
                if edge.waiters == 0 {
                    waits.remove(&self.key);
                }
            }
        }
    }
}
