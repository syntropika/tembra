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
pub(crate) type BeforeCreate = Rc<dyn Fn() -> LocalBoxFuture<'static, Result<Data>>>;
#[cfg(feature = "views")]
type Access = Rc<dyn Fn(&str) -> BTreeSet<String>>;

#[derive(Clone)]
pub(crate) struct Implementation {
    pub factory: Factory,
    pub local: BTreeMap<String, LocalMethod>,
    pub portable: BTreeMap<String, PortableMethod>,
    pub before_create: Option<BeforeCreate>,
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
                before_create: None,
                #[cfg(feature = "views")]
                access: None,
            },
            marker: std::marker::PhantomData,
        }
    }

    /// Wait for an adapter's executor before running the synchronous declaration factory.
    pub fn before_create<P, F, Fut>(mut self, prepare: F) -> Self
    where
        P: 'static,
        F: Fn() -> Fut + 'static,
        Fut: Future<Output = Result<P>> + 'static,
    {
        self.implementation.before_create = Some(Rc::new(move || {
            let ready = prepare();
            async move { Ok(Rc::new(ready.await?) as Data) }.boxed_local()
        }));
        self
    }

    /// Declare the canonical ABI signature used by typed dependency admission.
    pub fn signature(mut self, signature: Option<String>) -> Self {
        self.metadata.signature = signature;
        self
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

/// Native Orb declarations compile with the application and dispatch typed Rust callbacks.
pub struct Orb {
    pub(crate) metadata: core::Orb,
    pub(crate) factory: Factory,
    pub(crate) services: BTreeMap<String, Implementation>,
    #[cfg(feature = "hooks")]
    pub(crate) hooks: BTreeMap<String, crate::hooks::Signature>,
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
        #[cfg(feature = "hooks")]
        self.bind_hook_requirements();
        self
    }

    /// Mark an executor-backed orb whose every revision owns a private resource realm.
    pub fn isolated(mut self) -> Self {
        self.metadata.isolated = true;
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
        if let Some(signature) = &hook.wit {
            self.metadata
                .hook_signatures
                .insert(hook.name.clone(), signature.clone());
        }
        self.hooks.insert(hook.name.clone(), hook.identity());
        self.metadata.hooks.insert(hook.name);
        self
    }

    /// Register a WIT-checked executor callback in the ordinary contribution plan.
    #[doc(hidden)]
    pub fn contribute_portable<F, Fut>(
        mut self,
        target: impl Into<String>,
        hook: impl Into<String>,
        signature: impl Into<String>,
        priority: i32,
        handle: F,
    ) -> Self
    where
        F: Fn(Context, Value) -> Fut + 'static,
        Fut: Future<Output = Result<Option<Value>>> + 'static,
    {
        self.contributions.push(crate::hooks::Contribution {
            target: target.into(),
            hook: hook.into(),
            priority,
            signature: crate::hooks::Signature {
                native: None,
                wit: Some(signature.into()),
            },
            local: None,
            portable: Rc::new(move |context, value| handle(context, value).boxed_local()),
        });
        self.bind_hook_requirements();
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
        self.bind_hook_requirements();
        self
    }

    /// Keep WIT requirements in the kernel graph regardless of the builder's declaration order.
    fn bind_hook_requirements(&mut self) {
        for contribution in &self.contributions {
            let Some(signature) = &contribution.signature.wit else {
                continue;
            };
            for requirement in &mut self.metadata.requires {
                if requirement.orb == contribution.target
                    && requirement.hooks.contains(&contribution.hook)
                {
                    requirement
                        .hook_signatures
                        .entry(contribution.hook.clone())
                        .or_insert_with(|| signature.clone());
                }
            }
        }
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
