use crate::core::Reason;
use crate::{Context, Declaration, Error, Result, core};
use futures::{FutureExt, future::LocalBoxFuture};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{any::Any, collections::BTreeMap, future::Future, rc::Rc};

pub(crate) type Data = Rc<dyn Any>;
pub(crate) type Factory = Rc<dyn Fn(Declaration) -> Result<Data>>;
pub(crate) type LocalMethod =
    Rc<dyn Fn(Data, Context, Box<dyn Any>) -> LocalBoxFuture<'static, Result<Box<dyn Any>>>>;
pub(crate) type PortableMethod =
    Rc<dyn Fn(Data, Context, Value) -> LocalBoxFuture<'static, Result<Value>>>;
#[derive(Clone)]
pub(crate) struct Implementation {
    pub factory: Factory,
    pub local: BTreeMap<String, LocalMethod>,
    pub portable: BTreeMap<String, PortableMethod>,
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

/// Native facets are compiled with their application. No schema or generated compatibility file.
pub struct Facet {
    pub(crate) metadata: core::Facet,
    pub(crate) services: BTreeMap<String, Implementation>,
}

impl Facet {
    /// Create a native declaration with explicit identity and default options.
    pub fn new(name: impl Into<String>, api_version: u32) -> Self {
        Self {
            metadata: core::Facet::new(name, api_version),
            services: BTreeMap::new(),
        }
    }

    /// Add an explicit versioned facet dependency.
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
