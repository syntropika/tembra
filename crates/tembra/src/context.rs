use crate::{ContractToken, GetOptions, Result, core, token::input_value};

/// A view of an actual core call scope, preserving root, version, principal and cancellation.
///
/// Cloning never extends scope lifetime. Once the owning call settles, retained clones expire.
#[derive(Clone)]
pub struct Context {
    inner: core::Context,
}

impl Context {
    pub fn from_core(inner: core::Context) -> Self {
        Self { inner }
    }
    pub fn core(&self) -> &core::Context {
        &self.inner
    }

    /// Acquires a typed reference owned by this call scope.
    ///
    /// # Errors
    /// Returns serialization, contract, cancellation, expired/foreign-scope or readiness errors.
    /// Settlement releases the shared claim even if the generated wrapper is retained.
    pub async fn get<C: ContractToken>(
        &self,
        contract: C,
        input: C::Input,
    ) -> Result<C::Reference> {
        self.get_with_options(contract, input, GetOptions::default())
            .await
    }

    /// Acquires with explicit core principal/timeout options and the same scope ownership.
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
}

impl From<core::Context> for Context {
    fn from(inner: core::Context) -> Self {
        Self::from_core(inner)
    }
}

impl std::ops::Deref for Context {
    type Target = core::Context;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
