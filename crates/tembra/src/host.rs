use crate::core::Reason;
use crate::{
    Error, Facet, Result, State, core,
    facet::{Data, Implementation, type_error},
};
use futures::{
    FutureExt,
    channel::oneshot,
    future::{LocalBoxFuture, Shared},
    task::{LocalSpawn, LocalSpawnExt},
};
use serde::Serialize;
use serde_json::Value;
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    future::Future,
    rc::Rc,
};

type Encoder = Rc<dyn Fn() -> Result<Value>>;
struct ValueRecord {
    value: Data,
    portable: Option<Encoder>,
}
struct Object {
    environment: Data,
    implementation: Implementation,
    scope: core::Id,
    activation: Option<Shared<LocalBoxFuture<'static, Result<()>>>>,
}
type Opener = Box<dyn FnOnce() -> LocalBoxFuture<'static, Result<()>>>;
type Closer = Box<dyn FnOnce(Context) -> LocalBoxFuture<'static, Result<()>>>;

/// Cooperative cancellation never substitutes for physical opener settlement.
#[derive(Clone)]
pub struct ResourceSignal {
    cancelled: Rc<Cell<bool>>,
    wait: Shared<LocalBoxFuture<'static, ()>>,
}
impl ResourceSignal {
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.get()
    }
    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(Error::new(
                core::Code::Cancelled,
                "resource",
                "resource is closing",
            ))
        } else {
            Ok(())
        }
    }
    pub async fn cancelled(&self) {
        self.wait.clone().await;
    }
}

struct Inner {
    kernel: core::Host,
    executor: Rc<dyn LocalSpawn>,
    facets: RefCell<BTreeMap<core::Id, BTreeMap<String, Implementation>>>,
    objects: RefCell<BTreeMap<core::Id, Object>>,
    scope_objects: RefCell<BTreeMap<core::Id, core::Id>>,
    values: RefCell<BTreeMap<core::Id, ValueRecord>>,
    closers: RefCell<BTreeMap<core::Id, Closer>>,
    openers: RefCell<BTreeMap<core::Id, Opener>>,
    cancellations: RefCell<BTreeMap<core::Id, (ResourceSignal, oneshot::Sender<()>)>>,
    errors: RefCell<Vec<Error>>,
    external_actions: Cell<bool>,
    active: Cell<usize>,
    settled: RefCell<Vec<oneshot::Sender<()>>>,
    programs: RefCell<BTreeMap<core::Id, oneshot::Sender<Result<()>>>>,
}

/// A native adapter using an application-owned local executor. The executor must keep running
/// until physical work and cleanup settle, including after a caller drops its result future.
#[derive(Clone)]
pub struct Host(Rc<Inner>);
impl Host {
    pub fn new(executor: impl LocalSpawn + 'static) -> Self {
        Self::with_kernel(core::Host::new(), executor)
    }
    pub fn with_kernel(kernel: core::Host, executor: impl LocalSpawn + 'static) -> Self {
        Self(Rc::new(Inner {
            kernel,
            executor: Rc::new(executor),
            facets: RefCell::new(BTreeMap::new()),
            objects: RefCell::new(BTreeMap::new()),
            scope_objects: RefCell::new(BTreeMap::new()),
            values: RefCell::new(BTreeMap::new()),
            closers: RefCell::new(BTreeMap::new()),
            openers: RefCell::new(BTreeMap::new()),
            cancellations: RefCell::new(BTreeMap::new()),
            errors: RefCell::new(Vec::new()),
            external_actions: Cell::new(false),
            active: Cell::new(0),
            settled: RefCell::new(Vec::new()),
            programs: RefCell::new(BTreeMap::new()),
        }))
    }
    pub fn kernel(&self) -> core::Host {
        self.0.kernel.clone()
    }
    pub(crate) fn same(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
    pub fn has_facet(&self, id: core::Id) -> bool {
        self.0.facets.borrow().contains_key(&id)
    }
    pub fn delegate_actions(&self) {
        self.0.external_actions.set(true);
    }
    pub fn install(&self, facets: Vec<Facet>) -> Result<()> {
        let current = self.kernel().installed_facets();
        if facets.iter().any(|facet| {
            current
                .iter()
                .any(|installed| installed.declaration.name == facet.metadata.name)
        }) {
            return Err(Error::invalid(
                Reason::FacetInstalled,
                "compiled native facets cannot be replaced",
            ));
        }
        let reload = self.kernel().prepare(
            facets.iter().map(|facet| facet.metadata.clone()).collect(),
            Vec::new(),
        )?;
        self.kernel().commit(reload.id, Vec::new())?;
        for (facet, info) in facets.into_iter().zip(reload.facets) {
            self.0.facets.borrow_mut().insert(info.id, facet.services);
        }
        Ok(())
    }

    pub fn context(&self, root: core::Id, invocation: Option<core::Invocation>) -> Result<Context> {
        self.kernel().check_root(root)?;
        if invocation.as_ref().is_some_and(|call| call.root != root) {
            return Err(Error::invalid(
                Reason::Context,
                "invocation belongs to another root",
            ));
        }
        Ok(Context {
            host: self.clone(),
            root,
            call: invocation,
        })
    }
    pub async fn root<T>(
        &self,
        principal: &str,
        operation: impl FnOnce(Context) -> LocalBoxFuture<'static, Result<T>>,
    ) -> Result<T> {
        let root = self.kernel().begin_root(principal)?;
        let context = self.context(root, None)?;
        let _guard = RootGuard {
            host: self.clone(),
            root,
        };
        operation(context).await
    }
    pub fn get(&self, service: &str, key: Vec<String>, principal: &str) -> Result<Reference> {
        let acquired = self.kernel().acquire(
            core::Address {
                service: service.into(),
                key,
                principal: principal.into(),
            },
            0,
            BTreeSet::new(),
        )?;
        if acquired.activate {
            let scope = match self.activate(acquired.object, acquired.facet, service) {
                Ok(scope) => scope,
                Err(error) => {
                    self.kernel().fail(acquired.object)?;
                    self.kernel().release(acquired.reference)?;
                    self.flush();
                    return Err(error);
                }
            };
            let program = self.kernel().activate_scope(scope)?;
            let activation = self.run(program)?;
            self.0
                .objects
                .borrow_mut()
                .get_mut(&acquired.object)
                .expect("activated object")
                .activation = Some(activation);
        }
        Ok(Reference {
            host: self.clone(),
            id: acquired.reference,
            object: acquired.object,
            released: Cell::new(false),
        })
    }

    /// Used by a foreign language adapter after the shared kernel reserves the object.
    pub fn activate(&self, object: core::Id, facet: core::Id, service: &str) -> Result<core::Id> {
        if let Some(object) = self.0.objects.borrow().get(&object) {
            return Ok(object.scope);
        }
        let implementation = self
            .0
            .facets
            .borrow()
            .get(&facet)
            .and_then(|services| services.get(service))
            .cloned()
            .ok_or_else(|| Error::invalid(Reason::NativeService, service))?;
        let resources = Rc::new(RefCell::new(Vec::new()));
        let declaration = Declaration {
            host: self.clone(),
            object,
            resources: resources.clone(),
        };
        let result = (implementation.factory)(declaration);
        let scope = self.kernel().allocate_handle()?;
        self.kernel().register_scope(core::Scope {
            id: scope,
            owner: core::Owner::Object(object),
            facet,
            parents: Vec::new(),
            dependencies: Vec::new(),
            resources: resources.borrow().clone(),
            state_version: None,
            migrate: false,
            active: false,
            closing: false,
        })?;
        self.0.scope_objects.borrow_mut().insert(scope, object);
        self.0.objects.borrow_mut().insert(
            object,
            Object {
                environment: result.as_ref().cloned().unwrap_or_else(|_| Rc::new(())),
                implementation,
                scope,
                activation: None,
            },
        );
        result?;
        Ok(scope)
    }
    pub async fn invoke_portable(&self, call: core::Invocation, arguments: Value) -> Result<Value> {
        let (environment, handle) = {
            let objects = self.0.objects.borrow();
            let object = objects
                .get(&call.object)
                .ok_or_else(|| Error::invalid(Reason::NativeObject, "object is not activated"))?;
            (
                object.environment.clone(),
                object
                    .implementation
                    .portable
                    .get(&call.method)
                    .cloned()
                    .ok_or_else(|| Error::invalid(Reason::NativeMethod, &call.method))?,
            )
        };
        handle(environment, self.context(call.root, Some(call))?, arguments).await
    }
    pub fn portable_value(&self, id: core::Id) -> Result<Value> {
        let encoder = self
            .0
            .values
            .borrow()
            .get(&id)
            .and_then(|record| record.portable.clone())
            .ok_or_else(|| Error::invalid(Reason::LocalValue, "this Rust value is not portable"))?;
        encoder()
    }
    pub(crate) fn store<T: Serialize + 'static>(
        &self,
        value: T,
        portable: bool,
    ) -> Result<core::Id> {
        let id = self.kernel().allocate_handle()?;
        let value = Rc::new(value);
        let serialized = value.clone();
        let encoder: Option<Encoder> = portable.then(|| {
            Rc::new(move || {
                serde_json::to_value(&*serialized)
                    .map_err(|error| Error::invalid(Reason::Serialization, error.to_string()))
            }) as Encoder
        });
        self.0.values.borrow_mut().insert(
            id,
            ValueRecord {
                value,
                portable: encoder,
            },
        );
        Ok(id)
    }
    pub(crate) fn value<T: 'static>(&self, id: core::Id) -> Result<Rc<T>> {
        self.0
            .values
            .borrow()
            .get(&id)
            .ok_or_else(type_error)?
            .value
            .clone()
            .downcast()
            .map_err(|_| type_error())
    }
    pub fn drop_value(&self, id: core::Id) {
        self.0.values.borrow_mut().remove(&id);
    }

    fn spawn<T: 'static>(
        &self,
        operation: impl Future<Output = Result<T>> + 'static,
    ) -> Result<oneshot::Receiver<Result<T>>> {
        let (send, receive) = oneshot::channel();
        let host = self.clone();
        self.0.active.set(self.0.active.get() + 1);
        if let Err(error) = self.0.executor.spawn_local(async move {
            let _ = send.send(operation.await);
            host.0.active.set(host.0.active.get() - 1);
            for waiter in host.0.settled.take() {
                let _ = waiter.send(());
            }
        }) {
            self.0.active.set(self.0.active.get() - 1);
            return Err(Error::invalid(Reason::Executor, error.to_string()));
        }
        Ok(receive)
    }
    /// Drains native-owned actions. When sharing a kernel with JS, the JS adapter owns this drain
    /// and forwards disposal and value release instead, so actions are never consumed twice.
    pub fn flush(&self) {
        if self.0.external_actions.get() {
            return;
        }
        loop {
            let actions = self.kernel().drain();
            if actions.is_empty() {
                break;
            }
            for action in actions {
                match action {
                    core::Action::DropValue(id) => self.drop_value(id),
                    core::Action::DisposeObject(id) => {
                        let host = self.clone();
                        if let Err(error) = self.spawn(async move {
                            if let Err(error) = host.dispose(id).await {
                                host.0.errors.borrow_mut().push(error);
                            }
                            host.kernel().disposed(id)?;
                            host.flush();
                            Ok(())
                        }) {
                            self.0.errors.borrow_mut().push(error);
                        }
                    }
                    core::Action::Execute { program, effects } => {
                        // Bookkeeping and empty native hooks settle synchronously. Only user
                        // futures need executor tasks; the kernel still chooses every effect.
                        if effects.iter().all(|effect| {
                            !matches!(
                                effect,
                                core::Effect::Close { .. } | core::Effect::Open { .. }
                            )
                        }) {
                            let results = effects
                                .iter()
                                .map(|effect| completion(self.execute_immediate(effect)))
                                .collect();
                            if let Err(error) = self.kernel().complete_step(program, results) {
                                self.0.errors.borrow_mut().push(error);
                            }
                            continue;
                        }
                        let host = self.clone();
                        if let Err(error) = self.spawn(async move {
                            let results = futures::future::join_all(
                                effects.iter().map(|effect| host.execute_effect(effect)),
                            )
                            .await
                            .into_iter()
                            .map(completion)
                            .collect();
                            host.kernel().complete_step(program, results)?;
                            host.flush();
                            Ok(())
                        }) {
                            self.0.errors.borrow_mut().push(error);
                        }
                    }
                    core::Action::ProgramComplete { program, error } => {
                        if let Some(send) = self.0.programs.borrow_mut().remove(&program) {
                            let _ = send.send(error.map_or(Ok(()), Err));
                        }
                    }
                    core::Action::RetireFacet(id) => {
                        self.0.facets.borrow_mut().remove(&id);
                    }
                    core::Action::CancelEffects { effects, .. } => {
                        for effect in effects {
                            self.cancel_effect(&effect);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    pub async fn dispose(&self, object: core::Id) -> Result<()> {
        let scopes = self.kernel().scopes_for_owner(core::Owner::Object(object));
        if scopes.is_empty() {
            self.0.objects.borrow_mut().remove(&object);
            return Ok(());
        }
        let program = self.kernel().cleanup_scopes(scopes, None)?;
        self.run(program)?.await
    }

    fn run(&self, program: core::Id) -> Result<Shared<LocalBoxFuture<'static, Result<()>>>> {
        let (send, receive) = oneshot::channel();
        self.0.programs.borrow_mut().insert(program, send);
        self.kernel().run_program(program)?;
        self.flush();
        Ok(async move {
            receive
                .await
                .map_err(|_| Error::invalid(Reason::Executor, "engine completion was dropped"))?
        }
        .boxed_local()
        .shared())
    }

    /// Executes a native callback requested by the shared kernel, without planning cleanup.
    pub fn cancel_effect(&self, effect: &core::Effect) {
        if let core::Effect::Open { resource } | core::Effect::Restore { resource, .. } = effect {
            self.cancel_resource(*resource);
        }
    }
    fn cancel_resource(&self, resource: core::Id) {
        if let Some((signal, send)) = self.0.cancellations.borrow_mut().remove(&resource) {
            signal.cancelled.set(true);
            let _ = send.send(());
        }
    }
    pub async fn execute_effect(&self, effect: &core::Effect) -> Result<Option<core::Id>> {
        if let core::Effect::Open { resource } = effect {
            let open = self.0.openers.borrow_mut().remove(resource);
            if let Some(open) = open {
                open().await?;
            } else {
                self.kernel().resource_value(*resource)?;
            }
        } else if let core::Effect::Close { resource } = effect {
            self.cancel_resource(*resource);
            self.0.openers.borrow_mut().remove(resource);
            let close = self.0.closers.borrow_mut().remove(resource);
            let result = if let Some(close) = close {
                let root = self.kernel().cleanup_root("host")?;
                let _guard = RootGuard {
                    host: self.clone(),
                    root,
                };
                close(self.context(root, None)?).await
            } else {
                Ok(())
            };
            self.kernel().resource_closed(*resource)?;
            result?;
        } else {
            return self.execute_immediate(effect);
        }
        Ok(None)
    }

    fn execute_immediate(&self, effect: &core::Effect) -> Result<Option<core::Id>> {
        match effect {
            core::Effect::Drop { scope } => {
                if let Some(object) = self.0.scope_objects.borrow_mut().remove(scope) {
                    self.0.objects.borrow_mut().remove(&object);
                }
            }
            core::Effect::Stop { .. }
            | core::Effect::Prepare { .. }
            | core::Effect::Start { .. } => {}
            _ => {
                return Err(Error::invalid(
                    Reason::NativeEffect,
                    "effect has no registered native callback",
                ));
            }
        }
        Ok(None)
    }
    pub fn errors(&self) -> Vec<Error> {
        std::mem::take(&mut *self.0.errors.borrow_mut())
    }

    /// Waits for physical callbacks and all resource closers; cancellation cannot force termination.
    pub async fn close(&self) -> Result<()> {
        if self.0.external_actions.get() {
            return Err(Error::invalid(
                Reason::Driver,
                "the shared JS adapter owns host shutdown",
            ));
        }
        self.kernel().close();
        self.flush();
        while self.0.active.get() != 0 {
            let (send, receive) = oneshot::channel();
            self.0.settled.borrow_mut().push(send);
            let _ = receive.await;
            self.flush();
        }
        self.kernel().finish_close()?;
        self.0.values.borrow_mut().clear();
        self.0.facets.borrow_mut().clear();
        self.0.scope_objects.borrow_mut().clear();
        if let Some(error) = self.errors().into_iter().next() {
            return Err(error);
        }
        Ok(())
    }
}

struct RootGuard {
    host: Host,
    root: core::Id,
}
impl Drop for RootGuard {
    fn drop(&mut self) {
        if let Err(error) = self.host.kernel().end_root(self.root) {
            self.host.0.errors.borrow_mut().push(error);
        }
        self.host.flush();
    }
}

/// Contexts are issued by a host, and become unusable when their root or invocation ends.
#[derive(Clone)]
pub struct Context {
    pub(crate) host: Host,
    pub root: core::Id,
    call: Option<core::Invocation>,
}
impl Context {
    pub fn check(&self) -> Result<()> {
        self.host.kernel().check_root(self.root)?;
        if let Some(call) = &self.call {
            self.host.kernel().invocation(call.id)?;
        }
        Ok(())
    }
    pub fn cancel(&self) -> Result<()> {
        self.host.kernel().cancel_root(self.root)
    }
    pub fn principal(&self) -> Result<String> {
        self.host.kernel().root_principal(self.root)
    }
    pub fn invocation(&self) -> Result<core::Invocation> {
        self.call
            .clone()
            .ok_or_else(|| Error::invalid(Reason::Context, "an entered invocation is required"))
    }
}

pub struct Declaration {
    host: Host,
    object: core::Id,
    resources: Rc<RefCell<Vec<core::Id>>>,
}
impl Declaration {
    pub fn address(&self) -> Result<core::Address> {
        self.host.kernel().object_address(self.object)
    }
    pub fn state<T: Serialize + 'static>(
        &self,
        initial: T,
        version: u32,
        validate: impl Fn(&T) -> Result<()> + 'static,
    ) -> Result<State<T>> {
        State::new(self.host.clone(), self.object, initial, version, validate)
    }
    /// Declares a synchronous opener. The kernel runs it after the factory returns.
    pub fn resource<T: 'static>(
        &self,
        name: &str,
        version: u32,
        open: impl FnOnce() -> Result<T> + 'static,
        close: impl FnOnce(Rc<T>, Context) -> LocalBoxFuture<'static, Result<()>> + 'static,
    ) -> Result<Resource<T>> {
        self.resource_async(
            name,
            version,
            &[],
            move |_| async move { open() }.boxed_local(),
            close,
        )
    }

    /// Declares an asynchronous resource whose dependencies must physically open first.
    pub fn resource_async<T: 'static>(
        &self,
        name: &str,
        version: u32,
        dependencies: &[ResourceKey],
        open: impl FnOnce(ResourceSignal) -> LocalBoxFuture<'static, Result<T>> + 'static,
        close: impl FnOnce(Rc<T>, Context) -> LocalBoxFuture<'static, Result<()>> + 'static,
    ) -> Result<Resource<T>> {
        if dependencies
            .iter()
            .any(|dependency| !self.host.same(&dependency.host))
        {
            return Err(Error::invalid(
                Reason::ResourceHost,
                "resource dependencies belong to another host",
            ));
        }
        let record = self.host.kernel().resource(
            core::Owner::Object(self.object),
            name.into(),
            version,
            false,
            dependencies
                .iter()
                .map(|dependency| dependency.id)
                .collect(),
        )?;
        self.resources.borrow_mut().push(record.id);
        let host = self.host.clone();
        let resource = record.id;
        let (send, receive) = oneshot::channel();
        let signal = ResourceSignal {
            cancelled: Rc::new(Cell::new(false)),
            wait: async move {
                let _ = receive.await;
            }
            .boxed_local()
            .shared(),
        };
        self.host
            .0
            .cancellations
            .borrow_mut()
            .insert(resource, (signal.clone(), send));
        self.host.0.openers.borrow_mut().insert(
            resource,
            Box::new(move || {
                async move {
                    signal.check()?;
                    let value = Rc::new(open(signal.clone()).await?);
                    let id = host.kernel().allocate_handle()?;
                    host.0.values.borrow_mut().insert(
                        id,
                        ValueRecord {
                            value: value.clone(),
                            portable: None,
                        },
                    );
                    host.0
                        .closers
                        .borrow_mut()
                        .insert(resource, Box::new(move |context| close(value, context)));
                    if let Err(error) = host.kernel().resource_opened(resource, id) {
                        host.drop_value(id);
                        return Err(error);
                    }
                    signal.check()
                }
                .boxed_local()
            }),
        );
        Ok(Resource {
            key: ResourceKey {
                host: self.host.clone(),
                id: resource,
            },
            marker: std::marker::PhantomData,
        })
    }
}

#[derive(Clone)]
pub struct ResourceKey {
    host: Host,
    id: core::Id,
}
pub struct Resource<T> {
    key: ResourceKey,
    marker: std::marker::PhantomData<T>,
}
impl<T: 'static> Resource<T> {
    pub fn key(&self) -> ResourceKey {
        self.key.clone()
    }
    pub fn value(&self) -> Result<Rc<T>> {
        self.key
            .host
            .value(self.key.host.kernel().resource_value(self.key.id)?)
    }
}

/// A claim is released on drop. A running callback has its own physical kernel pin.
pub struct Reference {
    host: Host,
    id: core::Id,
    object: core::Id,
    released: Cell<bool>,
}
impl Reference {
    pub fn id(&self) -> core::Id {
        self.id
    }
    pub fn object(&self) -> core::Id {
        self.object
    }
    pub fn release(&self) -> Result<()> {
        if !self.released.replace(true) {
            self.host.kernel().release(self.id)?;
            self.host.kernel().forget_reference(self.id)?;
            self.host.flush();
        }
        Ok(())
    }
    /// Waits for the kernel's activation program, including every resource opener.
    pub async fn ready(&self) -> Result<()> {
        let ready = self
            .host
            .0
            .objects
            .borrow()
            .get(&self.object)
            .and_then(|object| object.activation.clone());
        if let Some(ready) = ready {
            ready.await?;
        }
        match self.host.kernel().status(self.id)? {
            core::Status::Live => Ok(()),
            _ => Err(Error::invalid(Reason::NotReady, "object is not live")),
        }
    }
    pub async fn call<A: 'static, R: 'static>(
        &self,
        context: &Context,
        method: &str,
        arguments: A,
    ) -> Result<R> {
        if !self.host.same(&context.host) {
            return Err(Error::invalid(Reason::Context, "foreign host context"));
        }
        context.check()?;
        self.ready().await?;
        context.check()?;
        let call = self
            .host
            .kernel()
            .begin_call(self.id, context.root, method)?;
        let selected = {
            let objects = self.host.0.objects.borrow();
            objects.get(&call.object).and_then(|object| {
                object
                    .implementation
                    .local
                    .get(method)
                    .map(|handle| (object.environment.clone(), handle.clone()))
            })
        };
        let Some((environment, handle)) = selected else {
            self.host.kernel().finish_call(call.id)?;
            return Err(Error::invalid(Reason::NativeMethod, method));
        };
        let host = self.host.clone();
        let entered = host.context(call.root, Some(call.clone()))?;
        let receive = self.host.spawn(async move {
            let result = handle(environment, entered, Box::new(arguments)).await;
            host.kernel().finish_call(call.id)?;
            host.flush();
            result
        })?;
        let result = receive.await.map_err(|_| {
            Error::invalid(Reason::Executor, "executor stopped before callback settled")
        })??;
        result
            .downcast::<R>()
            .map(|value| *value)
            .map_err(|_| type_error())
    }
}
impl Drop for Reference {
    fn drop(&mut self) {
        if let Err(error) = self.release() {
            self.host.0.errors.borrow_mut().push(error);
        }
    }
}

fn completion(result: Result<Option<core::Id>>) -> core::Completion {
    match result {
        Ok(value) => core::Completion { value, error: None },
        Err(error) => core::Completion {
            value: None,
            error: Some(error),
        },
    }
}
