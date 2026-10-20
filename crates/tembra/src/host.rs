use crate::{
    Context, ContractToken, Declaration, Driver, GetOptions, HostOptions, ProviderContract, Result,
    ServeOptions, core,
    token::{factory, input_value},
};
use serde::Serialize;
use std::{future::Future, rc::Rc};

/// A typed application facade over exactly one core Host.
///
/// Clones share the Host. References, object identity, versions and cleanup belong to the core.
#[derive(Clone)]
pub struct Host {
    inner: core::Host,
}

impl Host {
    pub fn new(options: HostOptions, driver: Rc<dyn Driver>) -> Self {
        Self::from_core(core::Host::new(options, driver))
    }

    /// Constructs a Host from a serializable application principal without JSON authoring.
    ///
    /// # Errors
    /// Returns portable serialization errors before creating the Host.
    pub fn with_driver(
        name: impl Into<String>,
        principal: impl Serialize,
        driver: Rc<dyn Driver>,
    ) -> Result<Self> {
        Ok(Self::new(
            HostOptions::new(name, input_value(&principal)?),
            driver,
        ))
    }

    pub fn from_core(inner: core::Host) -> Self {
        Self { inner }
    }
    pub fn core(&self) -> &core::Host {
        &self.inner
    }

    /// Acquires a ready managed reference inferred from the contract token.
    ///
    /// # Errors
    /// Rejects invalid inputs/contracts before activation and propagates readiness failures.
    /// Failure or dropping the acquisition future releases its claim; other claims and completed
    /// external effects remain independent. Reference clones share the resulting claim.
    pub async fn get<C: ContractToken>(
        &self,
        contract: C,
        input: C::Input,
    ) -> Result<C::Reference> {
        self.get_with_options(contract, input, GetOptions::default())
            .await
    }

    /// Acquires a typed reference with explicit advanced core options.
    ///
    /// # Errors
    /// Has the same failure effects as [`Self::get`].
    pub async fn get_with_options<C: ContractToken>(
        &self,
        contract: C,
        input: C::Input,
        options: GetOptions,
    ) -> Result<C::Reference> {
        let raw = self
            .inner
            .get(contract.descriptor()?, input_value(&input)?, options)
            .await?;
        contract.bind(raw)
    }

    /// Registers a synchronous factory checked against the token's generated provider trait.
    ///
    /// # Errors
    /// Refuses descriptor, duplicate-provider, placement and Host admission errors. Factory
    /// errors are observed during readiness; arbitrary factory effects are not rolled back.
    pub fn serve<C, P, F>(&self, contract: C, create: F) -> Result<()>
    where
        C: ProviderContract<P> + 'static,
        P: 'static,
        F: Fn(C::Input, serde_json::Value, &mut Declaration<'_>) -> Result<P> + 'static,
    {
        self.serve_with_options(contract, create, ServeOptions::default())
    }

    /// Registers the same typed factory with explicit lifecycle/adapter options.
    ///
    /// # Errors
    /// Has the admission and readiness failure effects of [`Self::serve`].
    pub fn serve_with_options<C, P, F>(
        &self,
        contract: C,
        create: F,
        options: ServeOptions,
    ) -> Result<()>
    where
        C: ProviderContract<P> + 'static,
        P: 'static,
        F: Fn(C::Input, serde_json::Value, &mut Declaration<'_>) -> Result<P> + 'static,
    {
        self.inner
            .serve(contract.descriptor()?, factory(contract, create), options)
    }

    /// Runs an async application root without requiring boxed futures.
    ///
    /// # Errors
    /// Returns closed/admission errors or the callback error. Completion or dropping the future
    /// expires this root and descendant scopes; cancellation does not undo external work.
    pub async fn root<T, F, Fut>(&self, callback: F) -> Result<T>
    where
        F: FnOnce(Context) -> Fut,
        Fut: Future<Output = Result<T>> + 'static,
    {
        self.inner
            .root(move |ctx| Box::pin(callback(ctx.into())))
            .await
    }

    /// Runs an application root under an explicit serializable principal.
    ///
    /// # Errors
    /// Returns principal serialization errors or the same failures as [`Self::root`].
    pub async fn root_as<T, F, Fut>(&self, principal: impl Serialize, callback: F) -> Result<T>
    where
        F: FnOnce(Context) -> Fut,
        Fut: Future<Output = Result<T>> + 'static,
    {
        self.inner
            .root_as(input_value(&principal)?, move |ctx| {
                Box::pin(callback(ctx.into()))
            })
            .await
    }

    /// Awaits managed Host shutdown, including core resource and physical invocation ownership.
    ///
    /// # Errors
    /// Reports cleanup failures after attempting every close; completed effects remain visible.
    pub async fn close(&self) -> Result<()> {
        self.inner.close().await
    }
}
