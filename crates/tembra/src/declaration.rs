//! Synchronous typed factory declarations backed by the shared core recorder.

use serde::{Serialize, de::DeserializeOwned};

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

    /// Instantiates a fresh value or initializer from setup-owned prepared metadata.
    /// # Errors
    /// Preserves actual Core declaration, name/version and lifecycle admission checks.
    pub fn state<T>(&mut self, declaration: StateDeclaration<T>) -> Result<Cell<T>> {
        let (name, inner) = declaration.into_parts();
        self.inner
            .state(name.to_string(), inner)
            .map(Cell::from_core)
    }

    /// Declares a fresh synchronous projection using setup-owned immutable metadata.
    /// # Errors
    /// Preserves current source, typed conversion, graph validation and atomic refusal checks.
    pub fn derive<D, T>(
        &mut self,
        definition: &crate::StateDefinition<T>,
        sources: D,
        compute: impl Fn(D::Values) -> Result<T> + 'static,
    ) -> Result<Readable<T>>
    where
        D: Sources,
        T: Serialize + DeserializeOwned + 'static,
    {
        self.inner
            .derive_definition(
                sources.core_sources()?,
                definition.core_definition(),
                move |snapshots| encode(&compute(D::values(snapshots)?)?),
            )
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
