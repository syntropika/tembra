use crate::core::Reason;
use crate::{
    Error, Orb, Result, State, core,
    orb::{Data, Implementation, type_error},
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

mod acquisition;
mod activation;
mod clock;
mod context;
mod declaration;
#[cfg(feature = "subscriptions")]
mod events;
mod execution;
#[cfg(feature = "hooks")]
mod hooks;
mod reference;
mod resources;
mod scopes;
mod signals;
mod tasks;
mod values;

pub use acquisition::AcquisitionOptions;
pub use context::Context;
pub use declaration::Declaration;
pub use reference::Reference;
pub use resources::{Cancellation, Resource, ResourceKey, ResourceSignal};
pub use signals::ForeignNotification;
pub use tasks::TaskScope;

type ForeignNotifier = Rc<dyn Fn(ForeignNotification)>;
type ForeignDispatcher = Rc<dyn Fn(Value) -> LocalBoxFuture<'static, Result<Value>>>;
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

struct Inner {
    #[cfg(feature = "hooks")]
    hooks: hooks::Hooks,
    #[cfg(feature = "subscriptions")]
    listeners: RefCell<BTreeMap<core::Id, Rc<events::Listener>>>,
    scopes: RefCell<BTreeMap<core::Id, scopes::NativeScope>>,
    host_scopes: RefCell<Vec<core::Id>>,
    scope_activations: RefCell<BTreeMap<core::Id, Shared<LocalBoxFuture<'static, Result<()>>>>>,
    orb_scopes: RefCell<BTreeMap<core::Id, core::Id>>,
    orb_factories: RefCell<BTreeMap<core::Id, crate::orb::Factory>>,
    orb_declaring: RefCell<BTreeSet<core::Id>>,
    orb_values: RefCell<BTreeMap<core::Id, Data>>,
    host_resources: RefCell<BTreeMap<String, ResourceKey>>,
    root_references: RefCell<BTreeMap<core::Id, Vec<Rc<Reference>>>>,
    task_signals: RefCell<BTreeMap<core::Id, Cancellation>>,
    clock: Rc<dyn crate::Clock>,
    clock_offset: Cell<u64>,
    orb_errors: RefCell<BTreeMap<core::Id, Error>>,
    deadline: RefCell<Option<(u64, futures::future::AbortHandle)>>,
    kernel: core::Host,
    executor: Rc<dyn LocalSpawn>,
    orbs: RefCell<BTreeMap<core::Id, BTreeMap<String, Implementation>>>,
    objects: RefCell<BTreeMap<core::Id, Object>>,
    scope_objects: RefCell<BTreeMap<core::Id, core::Id>>,
    observers: RefCell<BTreeMap<core::Id, Vec<std::rc::Weak<crate::readable::Observer>>>>,
    values: RefCell<BTreeMap<core::Id, ValueRecord>>,
    closers: RefCell<BTreeMap<core::Id, Closer>>,
    openers: RefCell<BTreeMap<core::Id, Opener>>,
    cancellations: RefCell<BTreeMap<core::Id, ResourceSignal>>,
    context_signals: RefCell<BTreeMap<core::Id, std::rc::Weak<resources::CancellationState>>>,
    errors: RefCell<Vec<Error>>,
    external_actions: Cell<bool>,
    driver_wake: RefCell<Option<Rc<dyn Fn()>>>,
    foreign_dispatcher: RefCell<Option<ForeignDispatcher>>,
    foreign_notifier: RefCell<Option<ForeignNotifier>>,
    flushing: Cell<bool>,
    active: Cell<usize>,
    settled_waker: futures::task::AtomicWaker,
    settled: RefCell<Vec<oneshot::Sender<()>>>,
    programs: RefCell<BTreeMap<core::Id, oneshot::Sender<Result<()>>>>,
}

/// Runs native callbacks on the supplied executor.
/// Keep the executor running until callbacks and cleanup settle.
#[derive(Clone)]
pub struct Host(Rc<Inner>);

impl Host {
    /// Create a native host driven by the supplied local executor.
    pub fn new(executor: impl LocalSpawn + 'static) -> Self {
        Self::with_kernel(core::Host::new(), executor)
    }

    /// Attach native callbacks to an existing shared kernel.
    pub fn with_kernel(kernel: core::Host, executor: impl LocalSpawn + 'static) -> Self {
        Self::with_clock(kernel, executor, crate::clock::SystemClock::default())
    }

    /// Create a host with an embedding-supplied monotonic clock and timer.
    pub fn with_clock(
        kernel: core::Host,
        executor: impl LocalSpawn + 'static,
        clock: impl crate::Clock + 'static,
    ) -> Self {
        Self(Rc::new(Inner {
            #[cfg(feature = "hooks")]
            hooks: hooks::Hooks::default(),
            #[cfg(feature = "subscriptions")]
            listeners: RefCell::new(BTreeMap::new()),
            scopes: RefCell::new(BTreeMap::new()),
            host_scopes: RefCell::new(Vec::new()),
            scope_activations: RefCell::new(BTreeMap::new()),
            orb_scopes: RefCell::new(BTreeMap::new()),
            orb_factories: RefCell::new(BTreeMap::new()),
            orb_declaring: RefCell::new(BTreeSet::new()),
            orb_values: RefCell::new(BTreeMap::new()),
            host_resources: RefCell::new(BTreeMap::new()),
            root_references: RefCell::new(BTreeMap::new()),
            task_signals: RefCell::new(BTreeMap::new()),
            clock: Rc::new(clock),
            clock_offset: Cell::new(0),
            orb_errors: RefCell::new(BTreeMap::new()),
            deadline: RefCell::new(None),
            kernel,
            executor: Rc::new(executor),
            orbs: RefCell::new(BTreeMap::new()),
            objects: RefCell::new(BTreeMap::new()),
            scope_objects: RefCell::new(BTreeMap::new()),
            observers: RefCell::new(BTreeMap::new()),
            values: RefCell::new(BTreeMap::new()),
            closers: RefCell::new(BTreeMap::new()),
            openers: RefCell::new(BTreeMap::new()),
            cancellations: RefCell::new(BTreeMap::new()),
            context_signals: RefCell::new(BTreeMap::new()),
            errors: RefCell::new(Vec::new()),
            external_actions: Cell::new(false),
            driver_wake: RefCell::new(None),
            foreign_dispatcher: RefCell::new(None),
            foreign_notifier: RefCell::new(None),
            flushing: Cell::new(false),
            active: Cell::new(0),
            settled_waker: futures::task::AtomicWaker::new(),
            settled: RefCell::new(Vec::new()),
            programs: RefCell::new(BTreeMap::new()),
        }))
    }

    /// Clone the shared kernel handle without copying its storage.
    pub fn kernel(&self) -> core::Host {
        self.0.kernel.clone()
    }

    /// Check whether two handles refer to the same native host.
    pub(crate) fn same(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }

    /// Check whether this adapter retains the orb implementation.
    pub fn has_orb(&self, id: core::Id) -> bool {
        self.0.orbs.borrow().contains_key(&id)
    }

    /// Give the shared JS adapter exclusive ownership of action dispatch.
    pub fn delegate_actions(&self) {
        self.0.external_actions.set(true);
        if let Some((_, timer)) = self.0.deadline.borrow_mut().take() {
            timer.abort();
        }
    }

    /// Install compiled native orbs without replacing existing implementations.
    pub fn install(&self, orbs: Vec<Orb>) -> Result<()> {
        let current = self.kernel().installed_orbs();
        if orbs.iter().any(|orb| {
            current
                .iter()
                .any(|installed| installed.declaration.name == orb.metadata.name)
        }) {
            return Err(Error::invalid(
                Reason::OrbInstalled,
                "compiled native orbs cannot be replaced",
            ));
        }
        #[cfg(feature = "hooks")]
        self.validate_hooks(&orbs)?;
        let reload = self.kernel().prepare(
            orbs.iter().map(|orb| orb.metadata.clone()).collect(),
            Vec::new(),
        )?;
        self.kernel().commit(reload.id, Vec::new())?;
        let mut factories = Vec::new();
        for (orb, info) in orbs.into_iter().zip(reload.orbs) {
            factories.push(info.id);
            self.0
                .orb_factories
                .borrow_mut()
                .insert(info.id, orb.factory);
            #[cfg(feature = "hooks")]
            self.install_hooks(info.id, orb.hooks, orb.contributions)?;
            self.0.orbs.borrow_mut().insert(info.id, orb.services);
        }
        for orb in factories {
            self.ensure_orb(orb)?;
        }
        let pinned = self
            .kernel()
            .installed_orbs()
            .into_iter()
            .filter(|info| self.has_orb(info.id))
            .flat_map(|info| info.declaration.services)
            .filter(|service| service.lifetime == core::Lifetime::Host)
            .collect::<Vec<_>>();
        for service in pinned {
            self.get(&service.name, vec!["main".into()], "host")?
                .release()?;
        }
        Ok(())
    }

    /// Acquire an object reference and schedule its native activation.
    pub fn get(&self, service: &str, key: Vec<String>, principal: &str) -> Result<Reference> {
        self.get_with_options(
            service,
            key,
            principal,
            crate::AcquisitionOptions::default(),
        )
    }

    /// Drain errors collected during callback execution and cleanup.
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
        self.finish_close()
    }

    /// Release native callback storage after the shared driver has physically settled shutdown.
    /// The kernel refuses premature completion without discarding active owners.
    pub fn finish_close(&self) -> Result<()> {
        self.kernel().finish_close()?;
        self.0.observers.borrow_mut().clear();
        self.0.scopes.borrow_mut().clear();
        self.0.driver_wake.borrow_mut().take();
        self.0.foreign_dispatcher.borrow_mut().take();
        self.0.root_references.borrow_mut().clear();
        self.0.values.borrow_mut().clear();
        self.0.orbs.borrow_mut().clear();
        self.0.orb_values.borrow_mut().clear();
        self.0.orb_errors.borrow_mut().clear();
        self.0.orb_factories.borrow_mut().clear();
        self.0.orb_scopes.borrow_mut().clear();
        self.0.scope_activations.borrow_mut().clear();
        self.0.host_resources.borrow_mut().clear();
        self.0.foreign_notifier.borrow_mut().take();
        #[cfg(feature = "hooks")]
        self.clear_hooks();
        self.0.scope_objects.borrow_mut().clear();
        if let Some(error) = self.errors().into_iter().next() {
            return Err(error);
        }
        Ok(())
    }
}
