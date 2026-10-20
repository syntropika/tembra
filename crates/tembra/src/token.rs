use crate::{Result, contracts::Contract, core};
use serde::{Serialize, de::DeserializeOwned};
use std::rc::Rc;

/// Connects one immutable consumer contract to its Rust input and managed reference.
///
/// Generated implementations check the consumer fingerprint. Multi-variant codegen checks the
/// selected variant when live and defers that check for loading declaration dependencies; the
/// contract macro emits a single-variant profile. Implementing this trait manually is an advanced
/// binding operation; it grants no permission to bypass core contract admission or create
/// implementation pointers.
pub trait ContractToken {
    /// The portable acquisition input whose normalized value participates in object identity.
    type Input: Serialize + DeserializeOwned;
    /// The managed typed consumer view generated for this token.
    type Reference;

    /// Builds or reuses the canonical descriptor without acquiring or registering an object.
    ///
    /// Macro and descriptor-generated tokens reuse their first successful immutable descriptor
    /// on each thread, retaining it until thread exit. Failed construction is retried.
    /// Schema generators must define stable metadata; changing it requires a new declaration.
    /// Input serialization still runs for every acquisition. Manually implemented tokens retain
    /// their own construction policy and are not placed in a shared generic cache.
    /// During thread teardown, an unavailable generated cache falls back to uncached construction.
    ///
    /// # Errors
    /// Returns contract/profile errors, without modifying a Host.
    fn descriptor(&self) -> Result<Rc<Contract>>;

    /// Wraps a real core claim with the declared consumer profile.
    ///
    /// # Errors
    /// Generated bindings return `ContractMismatch` for a different consumer fingerprint and
    /// `NotInVariant` for an incompatible live variant. Loading variant checks may be deferred
    /// until the generated operation observes actual shape. On refusal the passed claim is
    /// dropped; independent claims and already completed effects are unchanged.
    fn bind(&self, reference: core::Reference) -> Result<Self::Reference>;
}

/// Associates a generated token with a compile-time checked provider implementation.
///
/// The generated provider trait requires every method and state source. Conversion builds an
/// immutable core implementation candidate; it does not activate or install an object.
pub trait ProviderContract<P>: ContractToken {
    /// Converts the provider into managed core handlers and state sources.
    ///
    /// # Errors
    /// Returns invalid state-source or provider metadata errors. No Host is modified here;
    /// arbitrary effects performed by application provider accessors cannot be rolled back.
    fn implementation(&self, provider: P) -> Result<core::Implementation>;
}

pub(crate) fn input_value<T: Serialize>(input: &T) -> Result<serde_json::Value> {
    crate::contracts::to_portable_value(input)
        .map_err(|error| crate::RuntimeError::invalid(error.reason, error.message))
}

pub(crate) fn factory<C, P, F>(contract: C, factory: F) -> core::Factory
where
    C: ProviderContract<P> + 'static,
    F: Fn(C::Input, serde_json::Value, &mut crate::Declaration<'_>) -> Result<P> + 'static,
    P: 'static,
{
    Rc::new(move |input, principal, declaration| {
        let input = serde_json::from_value(input)
            .map_err(|error| crate::RuntimeError::invalid("input", error.to_string()))?;
        let provider = factory(
            input,
            principal,
            &mut crate::Declaration::from_core(declaration),
        )?;
        contract.implementation(provider)
    })
}
