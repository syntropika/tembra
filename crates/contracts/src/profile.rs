use std::sync::Arc;

use crate::{ContractError, Result, ValueSchema, canonical_json};

/// Exact canonical comparison data for one optional method position.
///
/// Clones share immutable bytes. This is neither a schema validator nor execution
/// authority: absence, native markers and portable schemas remain distinct.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanonicalProfileKey {
    kind: PositionKind,
    bytes: Arc<str>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PositionKind {
    Absent,
    Native,
    Portable,
}

impl CanonicalProfileKey {
    /// Prepares exact canonical bytes without compiling or normalizing a schema.
    ///
    /// # Errors
    /// Returns the existing serialization or canonical portable-domain error. The
    /// borrowed profile is unchanged; invalid schema syntax is not checked here.
    ///
    /// ```
    /// use serde_json::json;
    /// use tembra_contracts::{CanonicalProfileKey, ValueSchema};
    /// let a = Some(ValueSchema::Portable(json!({"minimum":1})));
    /// let b = Some(ValueSchema::Portable(json!({"minimum":1.0})));
    /// assert_eq!(CanonicalProfileKey::new(&a)?, CanonicalProfileKey::new(&b)?);
    /// assert_ne!(CanonicalProfileKey::new(&None)?, CanonicalProfileKey::new(&a)?);
    /// # Ok::<(), tembra_contracts::ContractError>(())
    /// ```
    pub fn new(profile: &Option<ValueSchema>) -> Result<Self> {
        let value = serde_json::to_value(profile)
            .map_err(|error| ContractError::new("serialization", error.to_string()))?;
        let kind = match profile {
            None => PositionKind::Absent,
            Some(ValueSchema::Native(_)) => PositionKind::Native,
            Some(ValueSchema::Portable(_)) => PositionKind::Portable,
        };
        canonical_json(&value).map(|bytes| Self {
            kind,
            bytes: Arc::from(bytes),
        })
    }
}
