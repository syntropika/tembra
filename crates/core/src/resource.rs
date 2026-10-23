use crate::{CancellationToken, Result, RuntimeError};
use futures::future::{FutureExt, LocalBoxFuture, Shared};
use std::{
    any::{Any, TypeId},
    cell::RefCell,
    marker::PhantomData,
    rc::{Rc, Weak},
};

use crate::{host::HostInner, reference::Reference};

mod reload;
pub(crate) use reload::{ResourceReload, ResourceSet};

#[derive(Clone)]
pub enum ResourceDependency {
    Reference(Reference),
    Resource(ResourceHandle),
}

#[derive(Clone)]
pub struct ResourceHandle(pub(crate) Rc<ResourceRecord>);

pub type ResourceCloser<T> = Rc<dyn Fn(Rc<T>) -> LocalBoxFuture<'static, Result<()>>>;
type AdoptionCheck<T> = Rc<dyn Fn(&T) -> Result<()>>;
type ErasedAdoptionCheck = Rc<dyn Fn(&dyn Any) -> Result<()>>;

/// Declares a native resource and its dependency ordering.
///
/// Open receives cooperative cancellation. A successful late open is still closed after
/// cancellation; close removes runtime access before invoking the callback and is attempted
/// once. Cleanup errors do not restore the value or roll back external effects.
pub struct ResourceDeclaration<T: 'static> {
    pub version: u64,
    pub exclusive: bool,
    pub deps: Vec<ResourceDependency>,
    pub open: Rc<dyn Fn(CancellationToken) -> LocalBoxFuture<'static, Result<T>>>,
    pub close: Option<ResourceCloser<T>>,
    adoption_check: Option<AdoptionCheck<T>>,
}

impl<T: 'static> ResourceDeclaration<T> {
    pub fn new(
        open: impl Fn(CancellationToken) -> LocalBoxFuture<'static, Result<T>> + 'static,
    ) -> Self {
        Self {
            version: 1,
            exclusive: false,
            deps: Vec::new(),
            open: Rc::new(open),
            close: None,
            adoption_check: None,
        }
    }

    pub fn with_close(
        mut self,
        close: impl Fn(Rc<T>) -> LocalBoxFuture<'static, Result<()>> + 'static,
    ) -> Self {
        self.close = Some(Rc::new(close));
        self
    }
    /// Checks whether an existing value may adopt this declaration's new callbacks.
    ///
    /// The check runs during reload preparation, after synchronous factory recording and
    /// before any gate or installed mutation. It must not mutate the resource.
    /// # Errors
    /// A returned error refuses adoption and leaves the installed declaration and value intact.
    ///
    /// Run `just run-example reload` for a complete example that asserts failed preparation
    /// preserves installed values, then commits migration and keeps one resource open.
    pub fn with_adoption_check(mut self, check: impl Fn(&T) -> Result<()> + 'static) -> Self {
        self.adoption_check = Some(Rc::new(check));
        self
    }
}

pub struct Resource<T: 'static> {
    pub(crate) record: Rc<ResourceRecord>,
    pub(crate) host: std::rc::Weak<HostInner>,
    marker: PhantomData<T>,
}

impl<T: 'static> Clone for Resource<T> {
    fn clone(&self) -> Self {
        Self {
            record: self.record.clone(),
            host: self.host.clone(),
            marker: PhantomData,
        }
    }
}

impl<T: 'static> Resource<T> {
    /// Returns a weak capability that does not retain this record or its callbacks.
    /// Adapter handle tables can upgrade it temporarily for each operation.
    pub fn downgrade(&self) -> WeakResource<T> {
        WeakResource {
            record: Rc::downgrade(&self.record),
            host: self.host.clone(),
            marker: PhantomData,
        }
    }

    /// Clones the open native value without changing its logical lifetime.
    ///
    /// # Errors
    /// Returns `not_ready` during factories or while not open, and `invalid/resource_type`
    /// for a type mismatch. Retained `Rc` values can outlive closure; callers must respect
    /// their resource's own usability rules after cleanup.
    pub fn value(&self) -> Result<Rc<T>> {
        if self
            .host
            .upgrade()
            .is_some_and(|host| host.in_factory.get() > 0)
        {
            return Err(RuntimeError::new(
                crate::ErrorCode::NotReady,
                "factory",
                "resource values cannot be read in a factory",
            ));
        }
        if let Some(cause) = self.record.unavailable.borrow().clone() {
            return Err(cause);
        }
        self.record
            .value
            .borrow()
            .clone()
            .ok_or_else(|| {
                RuntimeError::new(
                    crate::ErrorCode::NotReady,
                    "resource",
                    "resource is not open",
                )
            })?
            .downcast::<T>()
            .map_err(|_| {
                RuntimeError::invalid(
                    "resource_type",
                    "resource type does not match its declaration",
                )
            })
    }

    pub fn dependency(&self) -> ResourceDependency {
        ResourceDependency::Resource(ResourceHandle(self.record.clone()))
    }
    pub fn version(&self) -> u64 {
        self.record.spec().version
    }
    pub fn exclusive(&self) -> bool {
        self.record.spec().exclusive
    }
}

/// A weak resource capability, without ownership of its value or callback captures.
///
/// Upgrading creates an ordinary Resource temporarily; its factory/readiness/type checks
/// remain unchanged. A retained strong Resource can keep metadata alive after managed
/// closure, but does not restore a closed value. A weak handle alone retains neither.
///
/// ```
/// use std::{cell::RefCell, collections::BTreeMap, rc::Rc};
/// use futures::FutureExt;
/// use serde_json::json;
/// use tembra_core::{contracts::Contract, *};
/// let driver = ManualDriver::new();
/// let host = Host::new(HostOptions::new("example", json!(null)), driver.clone());
/// let token = Rc::new(Contract::local("resource", 1, json!({}), BTreeMap::new())?);
/// let saved = Rc::new(RefCell::new(None));
/// let captured = saved.clone();
/// host.serve(token.clone(), Rc::new(move |_, _, declaration| {
///     let resource = declaration.resource("value",
///         ResourceDeclaration::new(|_| async { Ok(7_u32) }.boxed_local()))?;
///     *captured.borrow_mut() = Some(resource.downgrade());
///     Ok(Implementation::new())
/// }), ServeOptions::default())?;
/// let reference = driver.run_until(host.get(token, json!({}), GetOptions::default()))?;
/// let weak = saved.borrow().clone().unwrap();
/// assert_eq!(*weak.upgrade()?.value()?, 7);
/// reference.release();
/// driver.run_until(host.close())?;
/// assert_eq!(weak.upgrade().err().unwrap().code, ErrorCode::Gone);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub struct WeakResource<T: 'static> {
    record: Weak<ResourceRecord>,
    host: Weak<HostInner>,
    marker: PhantomData<T>,
}
impl<T: 'static> Clone for WeakResource<T> {
    fn clone(&self) -> Self {
        Self {
            record: self.record.clone(),
            host: self.host.clone(),
            marker: PhantomData,
        }
    }
}
impl<T: 'static> WeakResource<T> {
    /// Obtains a temporary ordinary handle without opening or reviving the resource.
    ///
    /// # Errors
    /// Returns `gone/resource` when no strong record owner remains. Success does not imply
    /// that its value is open: use Resource::value for the existing lifecycle checks.
    pub fn upgrade(&self) -> Result<Resource<T>> {
        let record = self.record.upgrade().ok_or_else(|| {
            RuntimeError::new(
                crate::ErrorCode::Gone,
                "resource",
                "resource owner no longer exists",
            )
        })?;
        Ok(Resource {
            record,
            host: self.host.clone(),
            marker: PhantomData,
        })
    }
}

type OpenResource = Rc<dyn Fn(CancellationToken) -> LocalBoxFuture<'static, Result<Rc<dyn Any>>>>;
type CloseResource = Rc<dyn Fn(Rc<dyn Any>) -> LocalBoxFuture<'static, Result<()>>>;

pub(crate) struct ResourceSpec {
    pub version: u64,
    pub exclusive: bool,
    pub deps: Vec<ResourceDependency>,
    pub open: OpenResource,
    pub close: Option<CloseResource>,
    adoption_check: Option<ErasedAdoptionCheck>,
}

pub(crate) struct ResourceRecord {
    host: Weak<HostInner>,
    pub name: String,
    pub type_id: TypeId,
    spec: RefCell<Rc<ResourceSpec>>,
    pub value: RefCell<Option<Rc<dyn Any>>>,
    pub opening: RefCell<Option<Shared<LocalBoxFuture<'static, Result<()>>>>>,
    unavailable: RefCell<Option<RuntimeError>>,
}

impl ResourceRecord {
    pub fn declare<T: 'static>(
        name: String,
        declaration: ResourceDeclaration<T>,
        host: Weak<HostInner>,
    ) -> Resource<T> {
        let spec = ResourceSpec::erase(declaration);
        let record = Rc::new(Self {
            host: host.clone(),
            name,
            type_id: TypeId::of::<T>(),
            spec: RefCell::new(spec),
            value: RefCell::new(None),
            opening: RefCell::new(None),
            unavailable: RefCell::new(None),
        });
        Resource {
            record,
            host,
            marker: PhantomData,
        }
    }

    pub(crate) fn spec(&self) -> Rc<ResourceSpec> {
        self.spec.borrow().clone()
    }

    pub async fn close(&self) -> Result<()> {
        let observations = self
            .host
            .upgrade()
            .and_then(|host| host.observations.clone());
        let mut observation = crate::observability::start(
            &observations,
            crate::observability::Operation::ResourceClose,
            &self.name,
            None,
            None,
            None,
        );
        let result = async {
            let spec = self.spec();
            let value = self.value.borrow_mut().take();
            if let (Some(value), Some(close)) = (value, &spec.close) {
                close(value).await?;
            }
            Ok(())
        }
        .await;
        if let Some(observation) = &mut observation {
            observation.result(&result);
        }
        result
    }
}

impl ResourceSpec {
    fn erase<T: 'static>(declaration: ResourceDeclaration<T>) -> Rc<Self> {
        let open = declaration.open;
        let close = declaration.close.map(|close| {
            Rc::new(move |value: Rc<dyn Any>| {
                let close = close.clone();
                async move {
                    let value = value.downcast::<T>().map_err(|_| {
                        RuntimeError::invalid("resource_type", "resource close type mismatch")
                    })?;
                    close(value).await
                }
                .boxed_local()
            }) as CloseResource
        });
        let adoption_check = declaration.adoption_check.map(|check| {
            Rc::new(move |value: &dyn Any| {
                let value = value.downcast_ref::<T>().ok_or_else(|| {
                    RuntimeError::invalid("resource_type", "resource adoption type mismatch")
                })?;
                check(value)
            }) as Rc<dyn Fn(&dyn Any) -> Result<()>>
        });
        Rc::new(Self {
            version: declaration.version,
            exclusive: declaration.exclusive,
            deps: declaration.deps,
            open: Rc::new(move |token| {
                let future = open(token);
                async move { future.await.map(|value| Rc::new(value) as Rc<dyn Any>) }.boxed_local()
            }),
            close,
            adoption_check,
        })
    }
}
