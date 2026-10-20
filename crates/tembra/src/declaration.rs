//! Synchronous typed factory declarations backed by the shared core recorder.

use schemars::JsonSchema;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::state::{Sources, contract_error, encode};
use crate::{Cell, ContractToken, Readable, Result, StateDeclaration, core};

/// A borrowed synchronous factory declaration; it owns no second runtime or state graph.
pub struct Declaration<'a> {
    inner: &'a mut core::Declaration,
}

impl<'a> Declaration<'a> {
    pub fn from_core(inner: &'a mut core::Declaration) -> Self {
        Self { inner }
    }

    pub fn core_mut(&mut self) -> &mut core::Declaration {
        self.inner
    }

    /// Declares a named portable cell directly from an ordinary Rust value.
    ///
    /// # Errors
    /// Refuses generated schemas outside the portable profile, failed conversions,
    /// duplicate names and invalid core declarations. No initializer runs here.
    pub fn state<T>(&mut self, name: impl Into<String>, initial: T) -> Result<Cell<T>>
    where
        T: Serialize + DeserializeOwned + JsonSchema + 'static,
    {
        self.state_with(name, StateDeclaration::new(initial)?)
    }

    /// Declares typed state with explicit initialization, migration or version metadata.
    ///
    /// # Errors
    /// Refuses invalid core declarations without running asynchronous initializers.
    pub fn state_with<T>(
        &mut self,
        name: impl Into<String>,
        declaration: StateDeclaration<T>,
    ) -> Result<Cell<T>> {
        self.inner
            .state(name, declaration.into_core())
            .map(Cell::from_core)
    }

    /// Declares a synchronous projection over a tuple of typed core sources.
    ///
    /// # Errors
    /// Refuses invalid source bindings, schemas or output conversions. A subsequent
    /// internal projection failure refuses the complete pending mutation without events.
    /// External source changes remain independently authoritative if refresh fails.
    pub fn derive<D, T>(
        &mut self,
        sources: D,
        compute: impl Fn(D::Values) -> Result<T> + 'static,
    ) -> Result<Readable<T>>
    where
        D: Sources,
        T: Serialize + DeserializeOwned + JsonSchema + 'static,
    {
        let schema = tembra_contracts::schema_for::<T>().map_err(contract_error)?;
        self.derive_with_schema(sources, schema, compute)
    }

    /// Projects typed sources into an existing canonical contract state schema.
    ///
    /// Generated bindings use this boundary for output DTOs without `JsonSchema`.
    /// Output conversion and validation remain inside the core graph preparation.
    ///
    /// # Errors
    /// Refuses invalid schemas before resolving sources or computing a value. Source,
    /// conversion and projection errors follow the failure effects of [`Self::derive`].
    #[doc(hidden)]
    pub fn derive_with_schema<D, T>(
        &mut self,
        sources: D,
        schema: Value,
        compute: impl Fn(D::Values) -> Result<T> + 'static,
    ) -> Result<Readable<T>>
    where
        D: Sources,
        T: Serialize + DeserializeOwned + 'static,
    {
        let schema = tembra_contracts::normalize_schema(&schema).map_err(contract_error)?;
        self.inner
            .derive(sources.core_sources()?, schema, move |snapshots| {
                encode(&compute(D::values(snapshots)?)?)
            })
            .map(Readable::from_core)
    }

    /// Records an object-owned dependency; its readiness is managed during activation.
    ///
    /// # Errors
    /// Returns token/input conversion, binding, duplicate-contract and cycle errors.
    pub fn get<C: ContractToken>(&mut self, contract: C, input: C::Input) -> Result<C::Reference> {
        let descriptor = contract.descriptor()?;
        let input = tembra_contracts::to_portable_value(&input).map_err(contract_error)?;
        let reference = self
            .inner
            .get(descriptor, input, core::GetOptions::default())?;
        contract.bind(reference)
    }
}
