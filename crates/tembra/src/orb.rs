use crate::core::Reason;
use crate::{Context, Declaration, Error, Result, core};
use futures::{FutureExt, future::LocalBoxFuture};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
#[cfg(feature = "views")]
use std::collections::BTreeSet;
use std::{any::Any, collections::BTreeMap, future::Future, rc::Rc};

pub(crate) type Data = Rc<dyn Any>;
pub(crate) type Factory = Rc<dyn Fn(Declaration) -> Result<Data>>;
pub(crate) type LocalMethod =
    Rc<dyn Fn(Data, Context, Box<dyn Any>) -> LocalBoxFuture<'static, Result<Box<dyn Any>>>>;
pub(crate) type PortableMethod =
    Rc<dyn Fn(Data, Context, Value) -> LocalBoxFuture<'static, Result<Value>>>;
#[cfg(feature = "views")]
type Access = Rc<dyn Fn(&str) -> BTreeSet<String>>;

#[derive(Clone)]
pub(crate) struct Implementation {
    pub factory: Factory,
    pub local: BTreeMap<String, LocalMethod>,
    pub portable: BTreeMap<String, PortableMethod>,
    #[cfg(feature = "views")]
    pub access: Option<Access>,
}

/// A typed service. Methods execute once; only the portable entrypoint invokes serde.
pub struct Service<E> {
    pub(crate) metadata: core::Service,
    pub(crate) implementation: Implementation,
    marker: std::marker::PhantomData<E>,
}

impl<E: 'static> Service<E> {
    /// Create a native declaration with explicit identity and default options.
    pub fn new(
        name: impl Into<String>,
        create: impl Fn(Declaration) -> Result<E> + 'static,
    ) -> Self {
        Self {
            metadata: core::Service::new(name, std::iter::empty::<String>()),
            implementation: Implementation {
                factory: Rc::new(move |declaration| Ok(Rc::new(create(declaration)?) as Data)),
                local: BTreeMap::new(),
                portable: BTreeMap::new(),
                #[cfg(feature = "views")]
                access: None,
            },
            marker: std::marker::PhantomData,
        }
    }

    /// Set the ownership policy and optional idle retention period.
    pub fn lifetime(mut self, lifetime: core::Lifetime, linger_ms: u32) -> Self {
        self.metadata.lifetime = lifetime;
        self.metadata.linger_ms = linger_ms;
        self
    }

    /// Bind one callback to typed local and portable entrypoints.
    pub fn method<A, R, F, Fut>(mut self, name: impl Into<String>, handle: F) -> Self
    where
        A: DeserializeOwned + 'static,
        R: Serialize + 'static,
        F: Fn(Rc<E>, Context, A) -> Fut + 'static,
        Fut: Future<Output = Result<R>> + 'static,
    {
        let name = name.into();
        let handle = Rc::new(handle);
        let portable = handle.clone();
        self.implementation.local.insert(
            name.clone(),
            Rc::new(move |environment, context, arguments| {
                let result = environment
                    .downcast::<E>()
                    .map_err(|_| type_error())
                    .and_then(|environment| {
                        let arguments = arguments.downcast::<A>().map_err(|_| type_error())?;
                        Ok(handle(environment, context, *arguments))
                    });
                async move { Ok(Box::new(result?.await?) as Box<dyn Any>) }.boxed_local()
            }),
        );
        self.implementation.portable.insert(
            name.clone(),
            Rc::new(move |environment, context, arguments| {
                let result = environment
                    .downcast::<E>()
                    .map_err(|_| type_error())
                    .and_then(|environment| {
                        let arguments = serde_json::from_value(arguments).map_err(|error| {
                            Error::invalid(Reason::Arguments, error.to_string())
                        })?;
                        Ok(portable(environment, context, arguments))
                    });
                async move {
                    serde_json::to_value(result?.await?)
                        .map_err(|error| Error::invalid(Reason::Result, error.to_string()))
                }
                .boxed_local()
            }),
        );
        self.metadata.methods.insert(name);
        self
    }
}

/// Report an argument or result type mismatch at native dispatch.
pub(crate) fn type_error() -> Error {
    Error::invalid(
        Reason::RustType,
        "native argument or result type does not match the method",
    )
}

/// Native orbs are compiled with their application. No schema or generated compatibility file.
pub struct Orb {
    pub(crate) metadata: core::Orb,
    pub(crate) factory: Factory,
    pub(crate) services: BTreeMap<String, Implementation>,
    #[cfg(feature = "hooks")]
    pub(crate) hooks: BTreeMap<String, std::any::TypeId>,
    #[cfg(feature = "hooks")]
    pub(crate) contributions: Vec<crate::hooks::Contribution>,
}

impl Orb {
    /// Create a native declaration with explicit identity and default options.
    pub fn new(name: impl Into<String>, api_version: u32) -> Self {
        Self {
            metadata: core::Orb::new(name, api_version),
            factory: Rc::new(|_| Ok(Rc::new(()) as Data)),
            services: BTreeMap::new(),
            #[cfg(feature = "hooks")]
            hooks: BTreeMap::new(),
            #[cfg(feature = "hooks")]
            contributions: Vec::new(),
        }
    }

    /// Add an explicit versioned orb dependency.
    pub fn requires(mut self, requirement: core::Requirement) -> Self {
        self.metadata.requires.push(requirement);
        self
    }

    /// Attach a typed service and its callback implementation.
    pub fn service<E>(mut self, service: Service<E>) -> Self {
        self.services
            .insert(service.metadata.name.clone(), service.implementation);
        self.metadata.services.push(service.metadata);
        self
    }
}

#[cfg(feature = "hooks")]
impl Orb {
    /// Declare an orb-owned hook contract.
    pub fn hook<A: 'static, R: 'static>(mut self, hook: crate::Hook<A, R>) -> Self {
        self.hooks.insert(hook.name.clone(), hook.signature());
        self.metadata.hooks.insert(hook.name);
        self
    }

    /// Contribute a typed callback. External targets must also be listed in `requires`.
    /// Lower priorities run first; ties retain contribution registration order.
    pub fn contribute<A, R, F, Fut>(
        mut self,
        target: impl Into<String>,
        hook: &crate::Hook<A, R>,
        priority: i32,
        handle: F,
    ) -> Self
    where
        A: DeserializeOwned + 'static,
        R: 'static,
        F: Fn(Context, A) -> Fut + 'static,
        Fut: Future<Output = Result<R>> + 'static,
    {
        self.contributions
            .push(hook.contribution(target.into(), priority, handle));
        self
    }
}

impl Orb {
    /// Create one shared environment for this orb and declare orb-owned resources and work.
    pub fn create<T: 'static>(
        mut self,
        create: impl Fn(Declaration) -> Result<T> + 'static,
    ) -> Self {
        self.factory = Rc::new(move |declaration| Ok(Rc::new(create(declaration)?) as Data));
        self
    }
}

#[cfg(feature = "views")]
impl<E: 'static> Service<E> {
    /// Restrict acquired references to the methods permitted for their principal.
    pub fn access(mut self, policy: impl Fn(&str) -> BTreeSet<String> + 'static) -> Self {
        self.implementation.access = Some(Rc::new(policy));
        self
    }
}
