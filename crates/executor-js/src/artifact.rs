use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use tembra_core::{Result, RuntimeError, contracts};

/// Immutable executable source and its declared exact ESM export set.
/// Construction checks integrity; the engine also checks lexical and JavaScript syntax.
///
/// ```
/// use tembra_executor_js::BundledJsArtifact;
/// let artifact = BundledJsArtifact::new("counter", "export default {};", vec!["default".into()])?;
/// assert_eq!(artifact.exports().collect::<Vec<_>>(), vec!["default"]);
/// assert_eq!(artifact.sha256().len(), 64);
/// assert!(BundledJsArtifact::new("", "", vec![]).is_err());
/// assert_eq!(artifact.identity(), "counter");
/// # Ok::<(), tembra_core::RuntimeError>(())
/// ```
#[derive(Clone, Debug)]
pub struct BundledJsArtifact {
    pub(crate) identity: String,
    pub(crate) source: String,
    pub(crate) exports: BTreeSet<String>,
    digest: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    format: String,
    identity: String,
    source: String,
    sha256: String,
    exports: Vec<String>,
}
impl BundledJsArtifact {
    /// Computes the digest for embedding-owned compiled ESM.
    ///
    /// # Errors
    /// Refuses an empty/control identity or duplicate/empty export names. No code runs.
    pub fn new(
        identity: impl Into<String>,
        source: impl Into<String>,
        exports: Vec<String>,
    ) -> Result<Self> {
        let identity = identity.into();
        let source = source.into();
        if identity.is_empty() || identity.chars().any(char::is_control) {
            return Err(invalid("invalid artifact identity"));
        }
        let set: BTreeSet<_> = exports.iter().cloned().collect();
        if set.len() != exports.len() || set.iter().any(String::is_empty) {
            return Err(invalid("invalid export set"));
        }
        let digest = format!("{:x}", Sha256::digest(source.as_bytes()));
        Ok(Self {
            identity,
            source,
            exports: set,
            digest,
        })
    }
    /// Parses the shared v1 envelope and verifies its source digest before execution.
    ///
    /// # Errors
    /// Refuses malformed/duplicate JSON, unsupported format, metadata or digest mismatch.
    pub fn from_json(text: &str) -> Result<Self> {
        let value = contracts::parse_json(text).map_err(|e| invalid(&e.message))?;
        let wire: Wire = serde_json::from_value(value).map_err(|e| invalid(&e.to_string()))?;
        if wire.format != "tembra-esm-v1" {
            return Err(invalid("unsupported artifact format"));
        }
        let artifact = Self::new(wire.identity, wire.source, wire.exports)?;
        if artifact.digest != wire.sha256 {
            return Err(invalid("source digest mismatch"));
        }
        Ok(artifact)
    }
    pub fn identity(&self) -> &str {
        &self.identity
    }
    pub fn sha256(&self) -> &str {
        &self.digest
    }
    pub fn exports(&self) -> impl ExactSizeIterator<Item = &str> {
        self.exports.iter().map(String::as_str)
    }
}
fn invalid(message: &str) -> RuntimeError {
    RuntimeError::invalid("esm_artifact", message)
}
