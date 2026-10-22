//! Immutable schema preparation, never a State value or an execution capability.
use crate::Result;
use serde_json::Value;
use std::rc::Rc;

/// Shared schema metadata. Clones never share cell values, Contexts or execution authority.
/// Second-pass compilation failures remain deferred until actual value validation.
#[derive(Clone)]
pub struct StateDefinition {
    pub(crate) inner: Rc<super::graph::StateSchema>,
}
impl StateDefinition {
    /// Prepares the same normalized metadata and deferred validator as a dynamic declaration.
    /// # Errors
    /// Returns first-pass schema/profile failures without creating a cell or running a callback.
    pub fn new(schema: Value) -> Result<Self> {
        Ok(Self {
            inner: Rc::new(super::graph::StateGraph::schema(schema)?),
        })
    }
    /// Borrows immutable normalized metadata, without granting access to any State instance.
    pub fn schema(&self) -> Rc<Value> {
        self.inner.source.clone()
    }
    /// Checks every actual value, preserving portable-value refusal before deferred schema errors.
    /// # Errors
    /// Returns the existing portable-domain and schema-conformance errors; nothing is mutated.
    pub fn validate(&self, value: &Value) -> tembra_contracts::Result<()> {
        self.inner.validate(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn prepared_metadata_preserves_deferred_budget_and_value_error_precedence() {
        let properties = (0..9_000)
            .map(|index| (format!("a{index}"), json!({"type":"array"})))
            .collect::<serde_json::Map<_, _>>();
        let definition =
            StateDefinition::new(json!({"type":"object","properties":properties})).unwrap();
        for value in [json!({}), json!(9_007_199_254_740_992_u64)] {
            assert_eq!(
                definition.validate(&value),
                tembra_contracts::validate(&definition.schema(), &value)
            );
        }
        let integer = StateDefinition::new(json!({"type":"integer"})).unwrap();
        assert!(integer.validate(&json!("bad")).is_err());
        integer.validate(&json!(1)).unwrap();
        assert!(StateDefinition::new(json!({"type":"unknown"})).is_err());
    }
}
