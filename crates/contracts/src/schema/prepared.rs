use std::sync::{Arc, OnceLock};

use serde_json::{Value, json};

use super::{
    check_normalization, normalize_schema_with_limits, portable, transform, validate_inner,
};
use crate::{CompileLimits, Result};

/// An immutable owned schema whose source preparation is shared by its clones.
///
/// Each operation still checks the actual value's portable domain and interprets the
/// schema. Preparation is not an execution grant or a cache of validated values.
#[derive(Clone, Debug)]
pub struct PreparedSchema {
    inner: Arc<Prepared>,
}

#[derive(Debug)]
struct Prepared {
    schema: Value,
    cleaning: OnceLock<Result<()>>,
}

impl PreparedSchema {
    /// Prepare a schema with the default expansion limits, without changing its source.
    ///
    /// # Errors
    /// Returns the same schema/profile refusals as [`crate::normalize_schema`]. A
    /// validation-only union can prepare successfully yet refuse a later cleaning operation.
    ///
    /// ```
    /// use serde_json::json;
    /// use tembra_contracts::PreparedSchema;
    /// let mut source = json!({"type":"integer", "minimum":0});
    /// let prepared = PreparedSchema::new(&source)?;
    /// source["minimum"] = json!(10);
    /// prepared.validate(&json!(2))?;
    /// let value = json!(-1);
    /// assert!(prepared.validate(&value).is_err());
    /// assert_eq!(value, json!(-1));
    /// # Ok::<(), tembra_contracts::ContractError>(())
    /// ```
    pub fn new(schema: &Value) -> Result<Self> {
        Self::with_limits(schema, CompileLimits::default())
    }

    /// Prepare with explicit source expansion budgets; later validation uses that owned tree.
    ///
    /// # Errors
    /// Returns [`crate::normalize_schema_with_limits`] errors, without modifying the source.
    pub fn with_limits(schema: &Value, limits: CompileLimits) -> Result<Self> {
        normalize_schema_with_limits(schema, limits).map(Self::retained)
    }

    fn retained(schema: Value) -> Self {
        Self {
            inner: Arc::new(Prepared {
                schema,
                cleaning: OnceLock::new(),
            }),
        }
    }

    /// Borrow the normalized schema. No mutable source or per-value state is exposed.
    pub fn normalized(&self) -> &Value {
        &self.inner.schema
    }

    /// Validate without defaults, field cleaning, coercion or mutation of either input.
    ///
    /// # Errors
    /// Checks value portability first, then returns the existing interpreter's conformance
    /// errors with their reason, path and message. The prepared schema remains reusable.
    pub fn validate(&self, value: &Value) -> Result<()> {
        portable(value, 0)?;
        validate_inner(self.normalized(), value, "/", 0)
    }

    /// Clone and clean method arguments without defaults or acquisition null fallback.
    /// This does not validate the returned candidate.
    ///
    /// # Errors
    /// Checks value portability before cleaning eligibility. Ambiguous schema cleaning is
    /// refused here, never by validation-only preparation. No partial candidate escapes.
    pub fn clean_arguments(&self, value: &Value) -> Result<Value> {
        portable(value, 0)?;
        self.require_cleaning()?;
        transform(self.normalized(), value, false, false, 0)
    }

    /// Clone, default, clean and validate acquisition input; top-level null means `{}`.
    ///
    /// # Errors
    /// Checks value portability, cleaning eligibility and the complete normalized candidate
    /// in that order. Neither caller data nor the prepared schema is changed on refusal.
    pub fn normalize_input(&self, value: &Value) -> Result<Value> {
        portable(value, 0)?;
        self.require_cleaning()?;
        let fallback = json!({});
        let value = if value.is_null() { &fallback } else { value };
        let normalized = transform(self.normalized(), value, true, false, 0)?;
        validate_inner(self.normalized(), &normalized, "/", 0)?;
        Ok(normalized)
    }

    fn require_cleaning(&self) -> Result<()> {
        self.inner
            .cleaning
            .get_or_init(|| check_normalization(self.normalized()))
            .clone()
    }
}
