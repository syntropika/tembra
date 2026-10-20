use std::{
    cell::{Cell as Flag, RefCell},
    rc::{Rc, Weak},
};

use futures::future::LocalBoxFuture;
use serde_json::Value;

use crate::{Context, Result};

use super::{
    Readable, Snapshot, gone,
    graph::{Candidate, NodeId, ReadStamp, StateGraph},
};

pub(crate) type StateInitializer = Rc<dyn Fn(Context) -> LocalBoxFuture<'static, Result<Value>>>;
pub(super) type StateMigration = Rc<dyn Fn(Value, u64) -> Result<Value>>;

pub(super) enum Initial {
    Value(Value),
    Async(StateInitializer),
}

/// A candidate named cell declaration. Validation occurs when the declaration is installed.
pub struct StateDeclaration {
    pub(super) schema: Value,
    pub(super) version: u64,
    pub(super) initial: Initial,
    pub(super) migrate: Option<StateMigration>,
}

impl StateDeclaration {
    pub fn new(schema: Value, initial: Value) -> Self {
        Self {
            schema,
            version: 1,
            initial: Initial::Value(initial),
            migrate: None,
        }
    }

    pub fn with_initializer(
        schema: Value,
        initialize: impl Fn(Context) -> LocalBoxFuture<'static, Result<Value>> + 'static,
    ) -> Self {
        Self {
            schema,
            version: 1,
            initial: Initial::Async(Rc::new(initialize)),
            migrate: None,
        }
    }

    /// Migrates a private value copy from the previous declaration version during reload.
    /// The returned value and all prospective derivations are validated before adoption.
    /// # Errors
    /// A callback error refuses preparation without modifying installed state. External
    /// callback effects cannot be rolled back; the callback must remain synchronous.
    ///
    /// Run `just run-example reload` for a complete example that asserts failed preparation
    /// preserves installed values, then commits migration and keeps one resource open.
    pub fn with_migrate(mut self, migrate: impl Fn(Value, u64) -> Result<Value> + 'static) -> Self {
        self.migrate = Some(Rc::new(migrate));
        self
    }

    pub fn version(mut self, version: u64) -> Self {
        self.version = version;
        self
    }
}

/// Whole-value replacement uses a root operation unless structural diff is requested.
#[derive(Clone, Copy, Debug, Default)]
pub struct ReplaceOptions {
    pub diff: bool,
}

/// A checked declaration comparison; this does not execute a migration or reload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateMetadataChange {
    Unchanged,
    MigrationRequired,
}

pub(super) struct Metadata {
    pub name: String,
    pub schema: RefCell<Rc<Value>>,
    pub version: Flag<u64>,
}

/// A writable weak state capability; cloning it does not retain its managed object.
#[derive(Clone)]
pub struct Cell {
    pub(super) graph: Weak<StateGraph>,
    pub(super) node: NodeId,
    pub(super) metadata: Rc<Metadata>,
    pub(super) readable: Readable,
}

impl Cell {
    pub fn readable(&self) -> Readable {
        self.readable.clone()
    }

    /// Returns the initialized immutable value.
    ///
    /// # Errors
    /// Returns lifecycle, pending-initialization or preparation-reentrancy errors.
    pub fn snapshot(&self) -> Result<Snapshot> {
        self.readable.snapshot()
    }

    /// Reads the candidate initializer view authorized by this live Context.
    /// # Errors
    /// Has the admission, pending and cycle failures of [`Readable::snapshot_in`].
    pub fn snapshot_in(&self, context: &Context) -> Result<Snapshot> {
        self.readable.snapshot_in(context)
    }

    pub fn name(&self) -> &str {
        &self.metadata.name
    }
    pub fn schema(&self) -> Rc<Value> {
        self.metadata.schema.borrow().clone()
    }
    pub fn version(&self) -> u64 {
        self.metadata.version.get()
    }

    /// Checks prospective metadata without changing the cell or running a migration.
    /// A raised version requires migration even when the schema is unchanged.
    ///
    /// # Errors
    /// Refuses unsupported schemas, non-positive/unsafe or lowered versions, and a changed
    /// schema without a raised version. No initializer or application callback runs.
    pub fn compare_declaration(&self, schema: &Value, version: u64) -> Result<StateMetadataChange> {
        if version == 0 || version > super::MAX_REVISION || version < self.version() {
            return Err(crate::RuntimeError::invalid(
                "state_version",
                "state version cannot decrease or leave the positive safe-integer range",
            ));
        }
        let schema = tembra_contracts::normalize_schema(schema)
            .map_err(|error| crate::RuntimeError::invalid(error.reason, error.message))?;
        if version > self.version() {
            return Ok(StateMetadataChange::MigrationRequired);
        }
        let old = tembra_contracts::canonical_json(&self.schema())
            .map_err(|error| crate::RuntimeError::invalid(error.reason, error.message))?;
        let new = tembra_contracts::canonical_json(&schema)
            .map_err(|error| crate::RuntimeError::invalid(error.reason, error.message))?;
        if old != new {
            return Err(crate::RuntimeError::invalid(
                "state_schema_version",
                "a changed state schema requires a higher version",
            ));
        }
        Ok(StateMetadataChange::Unchanged)
    }

    /// Computes and validates a private draft plus all affected internal derivations.
    ///
    /// # Errors
    /// Refuses invalid/expired contexts, factory writes, pending or ended state, reentrant
    /// graph access, callback errors and invalid candidates. No graph value changes on error;
    /// callback effects outside the graph cannot be rolled back. A Ready write barrier or
    /// non-grandfathered object gate returns retryable [`crate::ErrorCode::UpgradeConflict`]
    /// before invoking the draft callback.
    ///
    /// # Panics
    /// Native callback panics propagate before adoption; RAII restores preparation admission.
    pub fn prepare_change(
        &self,
        ctx: &Context,
        change: impl FnOnce(&mut Value) -> Result<()>,
    ) -> Result<Prepared> {
        self.graph
            .upgrade()
            .ok_or_else(gone)?
            .prepare(self, ctx, true, |previous| {
                let mut candidate = previous.clone();
                change(&mut candidate)?;
                Ok(candidate)
            })
    }

    /// Prepares a replacement with the same atomic validation as [`Self::prepare_change`].
    ///
    /// # Errors
    /// Has the context, lifecycle and validation failures of [`Self::prepare_change`].
    pub fn prepare_replace(
        &self,
        ctx: &Context,
        value: Value,
        options: ReplaceOptions,
    ) -> Result<Prepared> {
        self.graph
            .upgrade()
            .ok_or_else(gone)?
            .prepare(self, ctx, options.diff, |_| Ok(value))
    }

    /// Adopts every prepared candidate before delivering any committed notification.
    ///
    /// # Errors
    /// Rejects foreign, stale, aborted, consumed, expired or cancelled preparations before
    /// changing authority. This context must be the exact initiating scope. Observer `Err`
    /// results are reported independently and do not change a successful result. A Ready
    /// write barrier or non-grandfathered object gate returns retryable
    /// [`crate::ErrorCode::UpgradeConflict`] before adoption.
    ///
    /// # Panics
    /// An observer panic propagates after adoption and can interrupt remaining delivery.
    pub fn adopt(&self, ctx: &Context, prepared: &Prepared) -> Result<Snapshot> {
        self.graph
            .upgrade()
            .ok_or_else(gone)?
            .adopt(self, ctx, prepared)
    }

    /// Prepares and adopts one synchronous draft. An empty change emits no event.
    ///
    /// # Errors
    /// Combines [`Self::prepare_change`] and [`Self::adopt`] errors without adopting a failed
    /// candidate. Application callback effects remain outside that guarantee.
    ///
    /// # Panics
    /// Mutation/derivation panics precede adoption; observer panics can follow adoption.
    ///
    /// Asynchronous mutation callbacks cannot satisfy this API:
    ///
    /// ```compile_fail
    /// fn asynchronous_change(cell: &tembra_core::Cell, ctx: &tembra_core::Context) {
    ///     let _ = cell.change(ctx, |_draft| async { Ok(()) });
    /// }
    /// ```
    pub fn change(
        &self,
        ctx: &Context,
        change: impl FnOnce(&mut Value) -> Result<()>,
    ) -> Result<Snapshot> {
        let prepared = self.prepare_change(ctx, change)?;
        self.adopt(ctx, &prepared)
    }

    /// Replaces the value. Defaults to one root Set; `diff: true` requests structural ops.
    ///
    /// # Errors
    /// Combines [`Self::prepare_replace`] and [`Self::adopt`] errors. A semantic no-op emits
    /// no event but invalidates competing preparations.
    ///
    /// # Panics
    /// Native derivation/observer panics have the same timing as [`Self::change`].
    pub fn replace(
        &self,
        ctx: &Context,
        value: Value,
        options: ReplaceOptions,
    ) -> Result<Snapshot> {
        let prepared = self.prepare_replace(ctx, value, options)?;
        self.adopt(ctx, &prepared)
    }
}

/// A non-cloneable private candidate tied to its original cell, scope and dependency read set.
pub struct Prepared {
    pub(super) graph: Weak<StateGraph>,
    pub(super) node: NodeId,
    pub(super) context: Context,
    pub(super) topology: u64,
    pub(super) reads: Vec<ReadStamp>,
    pub(super) candidates: Vec<Candidate>,
    pub(super) root_diff: bool,
    pub(super) status: Flag<PreparedStatus>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum PreparedStatus {
    Pending,
    Aborted,
    Consumed,
}

impl Prepared {
    /// Discards an unused candidate idempotently; authoritative values remain unchanged.
    pub fn abort(&self) {
        if self.status.get() == PreparedStatus::Pending {
            self.status.set(PreparedStatus::Aborted);
        }
    }
}
