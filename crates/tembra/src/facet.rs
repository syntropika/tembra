use crate::{Declaration, ProviderContract, Result, core, token::factory};
use std::rc::Rc;

/// A stable native facet using typed provider factories and the core's installation lifecycle.
#[derive(Clone)]
pub struct NativeFacet {
    inner: core::NativeFacet,
}

impl NativeFacet {
    /// Stores synchronous declarations; setup runs when the core installs the facet.
    ///
    /// # Errors
    /// Returns invalid facet-name errors before invoking setup or mutating a Host.
    pub fn new(
        name: impl Into<String>,
        uses: Vec<Rc<crate::contracts::Contract>>,
        setup: impl Fn(&core::FacetScope, &mut FacetDeclaration<'_, '_>) -> Result<()> + 'static,
    ) -> Result<Self> {
        Ok(Self {
            inner: core::NativeFacet::new(name, uses, move |scope, declaration| {
                setup(scope, &mut FacetDeclaration { inner: declaration })
            })?,
        })
    }

    pub fn core(&self) -> &core::NativeFacet {
        &self.inner
    }
}

impl From<NativeFacet> for core::NativeFacet {
    fn from(facet: NativeFacet) -> Self {
        facet.inner
    }
}

/// Setup-only typed declaration authority; retaining a facet scope cannot extend this borrow.
pub struct FacetDeclaration<'a, 'b> {
    inner: &'a mut core::FacetDeclaration<'b>,
}

impl<'b> FacetDeclaration<'_, 'b> {
    /// Registers a typed provider using default referenced-object lifecycle.
    ///
    /// # Errors
    /// Returns descriptor/admission errors before installation; activation errors are observed
    /// by the core startup waiter. Application factory effects cannot be rolled back.
    pub fn serve<C, P, F>(&mut self, contract: C, create: F) -> Result<()>
    where
        C: ProviderContract<P> + 'static,
        P: 'static,
        F: Fn(C::Input, serde_json::Value, &mut Declaration<'_>) -> Result<P> + 'static,
    {
        self.serve_with_options(contract, create, core::ServeOptions::default())
    }

    /// Registers a typed provider with explicit lifecycle and adapter options.
    ///
    /// # Errors
    /// Has the same failure effects as [`Self::serve`].
    pub fn serve_with_options<C, P, F>(
        &mut self,
        contract: C,
        create: F,
        options: core::ServeOptions,
    ) -> Result<()>
    where
        C: ProviderContract<P> + 'static,
        P: 'static,
        F: Fn(C::Input, serde_json::Value, &mut Declaration<'_>) -> Result<P> + 'static,
    {
        self.inner
            .serve(contract.descriptor()?, factory(contract, create), options)
    }

    pub fn core_mut(&mut self) -> &mut core::FacetDeclaration<'b> {
        self.inner
    }
}

impl crate::Host {
    /// Installs stable native declarations and returns the core-owned startup waiter.
    ///
    /// # Errors
    /// Refuses duplicate/name/admission or synchronous setup failures. Dropping the returned
    /// waiter abandons readiness observation; it does not undo installed declarations.
    pub fn install(
        &self,
        facet: NativeFacet,
        options: core::LoadOptions,
    ) -> Result<core::FacetStartup> {
        self.core().install(facet.inner, options)
    }
}
