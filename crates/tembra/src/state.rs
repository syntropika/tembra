//! Typed views of portable state owned exclusively by the shared core.
//!
//! State remains within the existing portable profile even for native local contracts.
//! Rust values are decoded copies; mutating one never mutates a snapshot or authority.

use std::{fmt, future::Future, marker::PhantomData};

use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::{Context, Result, RuntimeError, core};

pub use core::{ReplaceOptions, StateMetadataChange, Subscription};

pub(crate) fn contract_error(error: tembra_contracts::ContractError) -> RuntimeError {
    RuntimeError::invalid(error.reason, error.message)
}

pub(crate) fn decode<T: DeserializeOwned>(value: &Value) -> Result<T> {
    tembra_contracts::from_portable_value(value)
        .map_err(|error| RuntimeError::invalid("state_decode", error.to_string()))
}

/// Serializes once, then checks the exact portable representation adopted by the core.
pub(crate) fn encode<T: Serialize + DeserializeOwned>(value: &T) -> Result<Value> {
    let value = tembra_contracts::to_portable_value(value).map_err(contract_error)?;
    let encoded = tembra_contracts::canonical_json(&value).map_err(contract_error)?;
    let normalized = tembra_contracts::parse_json(&encoded).map_err(contract_error)?;
    let _: T = decode(&normalized)?;
    Ok(normalized)
}

/// An immutable core snapshot decoded as `T` on demand, without a second state store.
pub struct Snapshot<T> {
    inner: core::Snapshot,
    marker: PhantomData<fn() -> T>,
}

impl<T> fmt::Debug for Snapshot<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(formatter)
    }
}

impl<T> Clone for Snapshot<T> {
    fn clone(&self) -> Self {
        Self::from_core(self.inner.clone())
    }
}

impl<T> Snapshot<T> {
    pub fn from_core(inner: core::Snapshot) -> Self {
        Self {
            inner,
            marker: PhantomData,
        }
    }

    pub fn core(&self) -> &core::Snapshot {
        &self.inner
    }

    pub fn revision(&self) -> u64 {
        self.inner.revision()
    }
}

impl<T: DeserializeOwned> Snapshot<T> {
    /// Returns an owned decoded value; changing it cannot modify this snapshot.
    ///
    /// # Errors
    /// Returns `invalid/state_decode` if the retained value cannot be decoded as `T`.
    /// Custom deserializers execute on every call and may have their own external effects.
    pub fn value(&self) -> Result<T> {
        decode(self.inner.value())
    }
}

/// An initial snapshot, a checked core delta, or a stream reset with a typed snapshot.
pub enum StateEvent<T> {
    Snapshot(Snapshot<T>),
    Ops {
        delta: core::delta::DeltaBatch,
        snapshot: Snapshot<T>,
    },
    Reset(Snapshot<T>),
}

impl<T> StateEvent<T> {
    fn from_core(event: core::StateEvent) -> Self {
        match event {
            core::StateEvent::Snapshot(snapshot) => Self::Snapshot(Snapshot::from_core(snapshot)),
            core::StateEvent::Ops { delta, snapshot } => Self::Ops {
                delta,
                snapshot: Snapshot::from_core(snapshot),
            },
            core::StateEvent::Reset(snapshot) => Self::Reset(Snapshot::from_core(snapshot)),
        }
    }

    pub fn snapshot(&self) -> &Snapshot<T> {
        match self {
            Self::Snapshot(snapshot) | Self::Ops { snapshot, .. } | Self::Reset(snapshot) => {
                snapshot
            }
        }
    }
}

/// A weak readable capability; cloning it never retains a managed object claim.
pub struct Readable<T> {
    inner: Result<core::Readable>,
    marker: PhantomData<fn() -> T>,
}

impl<T> fmt::Debug for Readable<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Readable")
            .field("binding_error", &self.inner.as_ref().err())
            .finish_non_exhaustive()
    }
}

impl<T> Clone for Readable<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            marker: PhantomData,
        }
    }
}

impl<T> Readable<T> {
    pub fn from_core(inner: core::Readable) -> Self {
        Self::from_result(Ok(inner))
    }

    /// Retains a weak core source or a checked binding error for an infallible accessor.
    pub fn from_result(inner: Result<core::Readable>) -> Self {
        Self {
            inner,
            marker: PhantomData,
        }
    }

    /// Resolves a member now and retains only its weak readable or its binding error.
    /// A subsequent accessor call can resolve a member of a newly selected variant.
    pub fn from_reference(reference: core::Reference, name: impl AsRef<str>) -> Self {
        Self::from_result(reference.readable(name.as_ref()))
    }

    /// Returns the advanced weak core capability.
    ///
    /// # Errors
    /// Returns the original member-binding error for an unavailable state accessor.
    pub fn core(&self) -> Result<core::Readable> {
        self.inner.clone()
    }

    /// Returns the current immutable snapshot, without eagerly decoding it.
    ///
    /// # Errors
    /// Returns binding, lifecycle, initialization or graph-admission errors from the core.
    pub fn snapshot(&self) -> Result<Snapshot<T>> {
        self.core()?.snapshot().map(Snapshot::from_core)
    }

    /// Reads the initializer candidate view authorized by this actual context.
    ///
    /// # Errors
    /// Returns the context and lifecycle errors of the core's `snapshot_in` operation.
    pub fn snapshot_in(&self, context: &Context) -> Result<Snapshot<T>> {
        self.core()?
            .snapshot_in(context.core())
            .map(Snapshot::from_core)
    }

    /// Waits weakly for the first value without acquiring an object or reference claim.
    ///
    /// # Errors
    /// Returns member-binding, admission and termination errors from the core.
    pub async fn await_hydrated(&self) -> Result<Snapshot<T>> {
        self.core()?.await_hydrated().await.map(Snapshot::from_core)
    }
}

impl<T: DeserializeOwned + 'static> Readable<T> {
    /// Decodes the current value into an independent Rust value.
    ///
    /// # Errors
    /// Returns snapshot errors or `invalid/state_decode` for an incompatible Rust type.
    pub fn value(&self) -> Result<T> {
        self.snapshot()?.value()
    }

    /// Receives the current snapshot and each subsequent committed snapshot in core order.
    ///
    /// # Errors
    /// Refuses unavailable members or core subscription admission. Callback failures are
    /// reported by the core after adoption; they do not roll back a committed mutation.
    /// Native callback panics propagate and can interrupt remaining publication.
    pub fn subscribe(
        &self,
        callback: impl Fn(Snapshot<T>) -> Result<()> + 'static,
    ) -> Result<Subscription> {
        self.core()?
            .subscribe(move |snapshot| callback(Snapshot::from_core(snapshot)))
    }

    /// Receives initial, delta and reset events in the shared core publication order.
    ///
    /// # Errors
    /// Has the admission and callback failure behavior of [`Self::subscribe`].
    pub fn subscribe_events(
        &self,
        callback: impl Fn(StateEvent<T>) -> Result<()> + 'static,
    ) -> Result<Subscription> {
        self.core()?
            .subscribe_events(move |event| callback(StateEvent::from_core(event)))
    }
}

/// A weak writable capability over portable core state, viewed as Rust `T`.
pub struct Cell<T> {
    inner: core::Cell,
    marker: PhantomData<fn() -> T>,
}

impl<T> fmt::Debug for Cell<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Cell")
            .field("name", &self.name())
            .field("version", &self.version())
            .finish_non_exhaustive()
    }
}

impl<T> Clone for Cell<T> {
    fn clone(&self) -> Self {
        Self::from_core(self.inner.clone())
    }
}

impl<T> Cell<T> {
    pub fn from_core(inner: core::Cell) -> Self {
        Self {
            inner,
            marker: PhantomData,
        }
    }

    pub fn core(&self) -> &core::Cell {
        &self.inner
    }

    pub fn readable(&self) -> Readable<T> {
        Readable::from_core(self.inner.readable())
    }

    pub fn name(&self) -> &str {
        self.inner.name()
    }

    pub fn version(&self) -> u64 {
        self.inner.version()
    }

    /// Returns an immutable snapshot without decoding it after a commit.
    ///
    /// # Errors
    /// Returns core lifecycle, pending-initialization and graph-admission errors.
    pub fn snapshot(&self) -> Result<Snapshot<T>> {
        self.inner.snapshot().map(Snapshot::from_core)
    }

    /// Reads the private initializer view authorized by this context.
    ///
    /// # Errors
    /// Returns core context, admission, pending and lifecycle errors.
    pub fn snapshot_in(&self, context: &Context) -> Result<Snapshot<T>> {
        self.inner
            .snapshot_in(context.core())
            .map(Snapshot::from_core)
    }

    /// Adopts a candidate only in its exact originating cell, context and graph state.
    ///
    /// # Errors
    /// Refuses stale, foreign, consumed, aborted or expired candidates before adoption.
    /// Observer failures are reported separately after adoption; native observer panics
    /// can propagate after adoption. No Rust conversion executes after adoption here.
    pub fn adopt(&self, context: &Context, prepared: &Prepared<T>) -> Result<Snapshot<T>> {
        self.inner
            .adopt(context.core(), &prepared.inner)
            .map(Snapshot::from_core)
    }
}

impl<T: DeserializeOwned> Cell<T> {
    /// Returns the current value as an independent Rust value.
    ///
    /// # Errors
    /// Returns snapshot errors or `invalid/state_decode` on incompatible serialization.
    pub fn value(&self) -> Result<T> {
        self.snapshot()?.value()
    }
}

impl<T: Serialize + DeserializeOwned> Cell<T> {
    /// Prepares a synchronous Rust draft and all internal derivations in the core.
    ///
    /// # Guarantees
    /// Decode, callback, encode, schema and internal derivation errors leave all
    /// authoritative values and revisions unchanged and publish no graph events.
    /// External callback or custom serialization effects cannot be rolled back.
    ///
    /// # Errors
    /// Returns core admission errors, the callback error or typed conversion failures.
    /// Native mutation and derivation panics propagate before adoption.
    pub fn prepare_change(
        &self,
        context: &Context,
        change: impl FnOnce(&mut T) -> Result<()>,
    ) -> Result<Prepared<T>> {
        self.inner
            .prepare_change(context.core(), |draft| {
                let mut value = decode(draft)?;
                change(&mut value)?;
                *draft = encode(&value)?;
                Ok(())
            })
            .map(Prepared::from_core)
    }

    /// Prepares an owned replacement; structural diff is optional.
    ///
    /// # Errors
    /// Conversion or core validation failures leave authority unchanged. Custom serializer
    /// effects are external to this guarantee and may run before context admission.
    pub fn prepare_replace(
        &self,
        context: &Context,
        value: T,
        options: ReplaceOptions,
    ) -> Result<Prepared<T>> {
        let value = encode(&value)?;
        self.inner
            .prepare_replace(context.core(), value, options)
            .map(Prepared::from_core)
    }

    /// Commits a synchronous typed draft and returns the immutable committed snapshot.
    ///
    /// # Errors
    /// Combines preparation and adoption errors without committing a failed candidate.
    /// Native observer panics can propagate after adoption, as in the core API.
    ///
    /// A provider can change a typed draft under a managed call context. Refusing a
    /// private draft preserves the prior value and revision; adding zero is a no-op:
    ///
    /// ```
    /// use tembra::prelude::*;
    ///
    /// #[tembra::contract(local, name = "docs.checked-counter", version = 1)]
    /// trait Counter {
    ///     #[state]
    ///     fn count(&self) -> u64;
    ///     async fn increment(&self, by: u64) -> Result<u64>;
    /// }
    ///
    /// struct Service { count: Cell<u64> }
    /// impl CounterProvider for Service {
    ///     fn count(&self) -> Readable<u64> { self.count.readable() }
    ///     async fn increment(&self, by: u64, context: Context) -> Result<u64> {
    ///         self.count.change(&context, |value| {
    ///             *value += by;
    ///             if *value > 3 {
    ///                 return Err(RuntimeError::invalid("limit", "count exceeds three"));
    ///             }
    ///             Ok(())
    ///         })?.value()
    ///     }
    /// }
    ///
    /// let driver = tembra::ManualDriver::new();
    /// let host = Host::with_driver("checked-counter", (), driver.clone())?;
    /// host.serve(Counter, |_, _, declaration| {
    ///     Ok(Service { count: declaration.state("count", 0_u64)? })
    /// })?;
    /// driver.run_until(async {
    ///     let counter = host.get(Counter, CounterInput {}).await?;
    ///     host.root(move |context| async move {
    ///         let counter = counter.with_context(&context);
    ///         let initial = counter.count().snapshot()?;
    ///         assert_eq!(counter.increment(2).await?, 2);
    ///         let committed = counter.count().snapshot()?;
    ///         assert_eq!(counter.increment(2).await.unwrap_err().reason, "limit");
    ///         assert_eq!(counter.count().value()?, 2);
    ///         assert_eq!(counter.count().snapshot()?.revision(), committed.revision());
    ///         assert_eq!(counter.increment(0).await?, 2);
    ///         assert_eq!(counter.count().snapshot()?.revision(), committed.revision());
    ///         assert_eq!(initial.value()?, 0);
    ///         Ok(())
    ///     }).await?;
    ///     host.close().await
    /// })?;
    /// # Ok::<(), tembra::RuntimeError>(())
    /// ```
    ///
    /// The mutable draft cannot escape into an asynchronous callback:
    ///
    /// ```compile_fail
    /// fn asynchronous_change(cell: &tembra::Cell<u64>, context: &tembra::Context) {
    ///     let _ = cell.change(context, |value| async move {
    ///         *value += 1;
    ///         Ok(())
    ///     });
    /// }
    /// ```
    pub fn change(
        &self,
        context: &Context,
        change: impl FnOnce(&mut T) -> Result<()>,
    ) -> Result<Snapshot<T>> {
        let prepared = self.prepare_change(context, change)?;
        self.adopt(context, &prepared)
    }

    /// Replaces the value using one root operation.
    ///
    /// # Errors
    /// Has the conversion, admission and adoption errors of [`Self::prepare_replace`].
    /// A semantic no-op emits no event but invalidates competing preparations.
    pub fn replace(&self, context: &Context, value: T) -> Result<Snapshot<T>> {
        self.replace_with(context, value, ReplaceOptions::default())
    }

    /// Replaces the value, optionally publishing a structural diff.
    ///
    /// # Errors
    /// Has the conversion, admission and adoption errors of [`Self::prepare_replace`].
    pub fn replace_with(
        &self,
        context: &Context,
        value: T,
        options: ReplaceOptions,
    ) -> Result<Snapshot<T>> {
        let prepared = self.prepare_replace(context, value, options)?;
        self.adopt(context, &prepared)
    }
}

/// A non-cloneable checked candidate with the core's cell, scope and revision guards.
pub struct Prepared<T> {
    inner: core::Prepared,
    marker: PhantomData<fn() -> T>,
}

impl<T> fmt::Debug for Prepared<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Prepared").finish_non_exhaustive()
    }
}

impl<T> Prepared<T> {
    fn from_core(inner: core::Prepared) -> Self {
        Self {
            inner,
            marker: PhantomData,
        }
    }

    /// Discards this preparation idempotently without changing authoritative state.
    pub fn abort(&self) {
        self.inner.abort();
    }
}

/// A typed declaration retaining core initialization, migration and version semantics.
pub struct StateDeclaration<T> {
    inner: core::StateDeclaration,
    marker: PhantomData<fn() -> T>,
}

impl<T> fmt::Debug for StateDeclaration<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StateDeclaration")
            .finish_non_exhaustive()
    }
}

impl<T: Serialize + DeserializeOwned + schemars::JsonSchema + 'static> StateDeclaration<T> {
    /// Builds a checked portable initial value without installing it in an object.
    ///
    /// # Errors
    /// Refuses unsupported schemas, nonportable values and failed Rust conversions.
    ///
    /// ```
    /// use tembra::StateDeclaration;
    ///
    /// let empty = Vec::<u64>::new();
    /// assert!(StateDeclaration::new(empty).is_ok());
    /// assert!(StateDeclaration::new(f64::NAN).is_err());
    /// assert!(StateDeclaration::new(u64::MAX).is_err());
    /// ```
    pub fn new(initial: T) -> Result<Self> {
        let schema = tembra_contracts::schema_for::<T>().map_err(contract_error)?;
        Self::with_schema(schema, initial)
    }

    /// Records an initializer that starts under the core's activation lifecycle.
    ///
    /// # Errors
    /// Refuses an unsupported generated schema now. Initialization and conversion failures
    /// later fail activation; cancellation still waits for started initializers to settle.
    pub fn with_initializer<F, Fut>(initialize: F) -> Result<Self>
    where
        F: Fn(Context) -> Fut + 'static,
        Fut: Future<Output = Result<T>> + 'static,
    {
        let schema = tembra_contracts::schema_for::<T>().map_err(contract_error)?;
        Self::with_schema_initializer(schema, initialize)
    }
}

impl<T: Serialize + DeserializeOwned + 'static> StateDeclaration<T> {
    /// Builds a typed declaration against an existing canonical contract state schema.
    ///
    /// This advanced binding boundary supports generated DTOs without `JsonSchema`.
    /// The supplied schema is normalized and the normalized initial value is validated
    /// before constructing the core declaration. Installing it remains core-owned.
    ///
    /// # Errors
    /// Refuses invalid or unsupported schemas, failed Rust conversions and initial values
    /// outside the supplied schema without recording state or running an initializer.
    #[doc(hidden)]
    pub fn with_schema(schema: Value, initial: T) -> Result<Self> {
        let schema = tembra_contracts::normalize_schema(&schema).map_err(contract_error)?;
        let value = encode(&initial)?;
        tembra_contracts::validate(&schema, &value).map_err(contract_error)?;
        Ok(Self {
            inner: core::StateDeclaration::new(schema, value),
            marker: PhantomData,
        })
    }

    /// Records a typed initializer against an existing canonical contract state schema.
    ///
    /// # Errors
    /// Refuses invalid or unsupported schemas immediately. Conversion and schema failures
    /// from the eventual value fail core activation before state becomes authoritative.
    /// Started initialization still follows core cancellation and physical cleanup rules.
    #[doc(hidden)]
    pub fn with_schema_initializer<F, Fut>(schema: Value, initialize: F) -> Result<Self>
    where
        F: Fn(Context) -> Fut + 'static,
        Fut: Future<Output = Result<T>> + 'static,
    {
        let schema = tembra_contracts::normalize_schema(&schema).map_err(contract_error)?;
        Ok(Self {
            inner: core::StateDeclaration::with_initializer(schema, move |context| {
                let future = initialize(context.into());
                Box::pin(async move { encode(&future.await?) })
            }),
            marker: PhantomData,
        })
    }

    /// Migrates a decoded old-version value into the new Rust representation.
    /// Decode and encode occur inside the core's synchronous reload preparation.
    pub fn with_migrate<Old: DeserializeOwned + 'static>(
        mut self,
        migrate: impl Fn(Old, u64) -> Result<T> + 'static,
    ) -> Self {
        self.inner = self
            .inner
            .with_migrate(move |value, version| encode(&migrate(decode(&value)?, version)?));
        self
    }
}

impl<T> StateDeclaration<T> {
    pub fn version(mut self, version: u64) -> Self {
        self.inner = self.inner.version(version);
        self
    }

    pub fn into_core(self) -> core::StateDeclaration {
        self.inner
    }
}

mod sealed {
    pub trait Sealed {}
}

/// A fixed tuple of typed sources for one synchronous core derivation.
pub trait Sources: sealed::Sealed + 'static {
    type Values;

    /// Resolves the existing core sources without acquiring additional object claims.
    ///
    /// # Errors
    /// Returns an unavailable member's binding error.
    fn core_sources(&self) -> Result<Vec<core::Readable>>;

    /// Decodes the graph's staged source snapshots.
    ///
    /// # Errors
    /// Returns a Rust conversion error; the core rejects the entire internal preparation.
    fn values(snapshots: &[core::Snapshot]) -> Result<Self::Values>;
}

impl sealed::Sealed for () {}

impl Sources for () {
    type Values = ();

    fn core_sources(&self) -> Result<Vec<core::Readable>> {
        Ok(Vec::new())
    }

    fn values(_snapshots: &[core::Snapshot]) -> Result<Self::Values> {
        Ok(())
    }
}

macro_rules! tuple_sources {
    ($($ty:ident:$index:tt),+) => {
        impl<$($ty),+> sealed::Sealed for ($(Readable<$ty>,)+) {}

        impl<$($ty: DeserializeOwned + 'static),+> Sources for ($(Readable<$ty>,)+) {
            type Values = ($($ty,)+);

            fn core_sources(&self) -> Result<Vec<core::Readable>> {
                Ok(vec![$(self.$index.core()?,)+])
            }

            fn values(snapshots: &[core::Snapshot]) -> Result<Self::Values> {
                Ok(($(decode(snapshots.get($index).ok_or_else(|| {
                    RuntimeError::invalid("state_sources", "missing derivation source")
                })?.value())?,)+))
            }
        }
    };
}

tuple_sources!(A:0);
tuple_sources!(A:0, B:1);
tuple_sources!(A:0, B:1, C:2);
tuple_sources!(A:0, B:1, C:2, D:3);
tuple_sources!(A:0, B:1, C:2, D:3, E:4);
tuple_sources!(A:0, B:1, C:2, D:3, E:4, F:5);
tuple_sources!(A:0, B:1, C:2, D:3, E:4, F:5, G:6);
tuple_sources!(A:0, B:1, C:2, D:3, E:4, F:5, G:6, H:7);
