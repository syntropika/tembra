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

mod activation;
mod context;
mod declaration;
mod execution;
mod reference;
mod resources;
mod values;

pub use context::Context;
pub use declaration::Declaration;
pub use reference::Reference;
pub use resources::{Resource, ResourceKey, ResourceSignal};

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

    /// Clone the shared kernel handle without copying its storage.
    pub fn kernel(&self) -> core::Host {
        self.0.kernel.clone()
    }

    /// Check whether two handles refer to the same native host.
    pub(crate) fn same(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }

    /// Check whether this adapter retains the facet implementation.
    pub fn has_facet(&self, id: core::Id) -> bool {
        self.0.facets.borrow().contains_key(&id)
    }

    /// Give the shared JS adapter exclusive ownership of action dispatch.
    pub fn delegate_actions(&self) {
        self.0.external_actions.set(true);
    }

    /// Install compiled native facets without replacing existing implementations.
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

    /// Acquire an object reference and schedule its native activation.
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
