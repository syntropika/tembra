//! Portable contracts, strict JSON validation, and stable object identity.

mod builder;
mod coverage;
mod descriptor;
mod json;
mod profile;
mod schema;
mod serialization;
mod source;

pub use builder::{ContractBuilder, schema_for};
pub use coverage::{identity_compatible, input_compatible, output_fits};
pub use descriptor::{
    AcquisitionInput, Contract, ContractKind, Descriptor, Member, Placement, ValueSchema,
};
pub use json::parse_json;
pub use profile::CanonicalProfileKey;
pub use schema::{
    PreparedSchema, canonical_json, clean_arguments, normalize_input, normalize_schema,
    normalize_schema_with_limits, validate,
};
pub use serialization::{from_portable_value, to_portable_value};
pub use source::CompileLimits;

/// A contract error with a stable machine-readable reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContractError {
    pub reason: String,
    /// Schema or descriptor location; `/` denotes the root when unavailable.
    pub path: String,
    pub message: String,
}

impl ContractError {
    pub fn new(reason: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            path: "/".into(),
            message: message.into(),
        }
    }

    /// Attach the failing schema/descriptor location without parsing display text.
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = path.into();
        self
    }
}

impl std::fmt::Display for ContractError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.reason, self.message)
    }
}

impl std::error::Error for ContractError {}

pub type Result<T> = std::result::Result<T, ContractError>;
