use crate::{ExecutableFacet, ReloadOptions, ReloadReport, Result, RuntimeError};
use futures::future::LocalBoxFuture;
use serde::{Deserialize, Serialize};
use std::{rc::Rc, time::Duration};

/// An embedding-owned executable artifact identity. The runtime does not fetch it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "FacetDto", into = "FacetDto")]
pub struct RemoteFacet {
    name: String,
    artifact: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FacetDto {
    name: String,
    artifact: String,
}
impl RemoteFacet {
    /// Checks metadata without loading or evaluating application code.
    ///
    /// # Errors
    /// Refuses empty/control-containing names, names above 256 bytes and artifact
    /// identities above 1024 bytes. Neither field grants filesystem/network authority.
    pub fn new(name: impl Into<String>, artifact: impl Into<String>) -> Result<Self> {
        let name = name.into();
        let artifact = artifact.into();
        check_name(&name, 256)?;
        check_name(&artifact, 1024)?;
        Ok(Self { name, artifact })
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn artifact(&self) -> &str {
        &self.artifact
    }
}
impl TryFrom<FacetDto> for RemoteFacet {
    type Error = RuntimeError;
    fn try_from(value: FacetDto) -> Result<Self> {
        Self::new(value.name, value.artifact)
    }
}
impl From<RemoteFacet> for FacetDto {
    fn from(value: RemoteFacet) -> Self {
        Self {
            name: value.name,
            artifact: value.artifact,
        }
    }
}

/// Embedding-supplied loading of one ready foreign facet; no built-in resolver is implied.
pub type ReloadFacetLoader =
    Rc<dyn Fn(RemoteFacet) -> LocalBoxFuture<'static, Result<ExecutableFacet>>>;

/// A participant's coherent-installation observation, not physical cleanup acknowledgement.
#[derive(Clone, Debug)]
pub struct ParticipantReloadReport {
    pub(super) name: String,
    pub(super) version: u64,
    pub(super) prepared: usize,
    pub(super) gated: usize,
}
impl ParticipantReloadReport {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn code_version(&self) -> u64 {
        self.version
    }
    pub fn prepared_objects(&self) -> usize {
        self.prepared
    }
    pub fn gated_objects(&self) -> usize {
        self.gated
    }
}

/// Completed local installation and direct-child acknowledgements; decisions are not durable.
#[derive(Clone, Debug)]
pub struct ClusterReloadReport {
    pub(super) local: ReloadReport,
    pub(super) children: Vec<ParticipantReloadReport>,
}
impl ClusterReloadReport {
    pub fn local(&self) -> &ReloadReport {
        &self.local
    }
    pub fn children(&self) -> &[ParticipantReloadReport] {
        &self.children
    }
}

pub(super) fn check_name(value: &str, maximum: usize) -> Result<()> {
    if value.is_empty() || value.len() > maximum || value.chars().any(char::is_control) {
        return Err(RuntimeError::invalid(
            "reload_artifact",
            "reload names must be bounded, nonempty and free of controls",
        ));
    }
    Ok(())
}
pub(crate) fn validate_batch(facets: &[RemoteFacet], removed: &[String]) -> Result<()> {
    use std::collections::BTreeSet;
    let mut names = BTreeSet::new();
    let mut bytes = 0_usize;
    if facets.len() > 128 || removed.len() > 128 {
        return Err(RuntimeError::invalid(
            "reload_artifact",
            "too many remote facets",
        ));
    }
    for facet in facets {
        if !names.insert(facet.name()) {
            return Err(RuntimeError::invalid(
                "reload_artifact",
                "duplicate remote facet",
            ));
        }
        bytes += facet.name.len() + facet.artifact.len();
    }
    for name in removed {
        check_name(name, 256)?;
        if !names.insert(name) {
            return Err(RuntimeError::invalid(
                "reload_artifact",
                "duplicate or simultaneously replaced and removed remote facet",
            ));
        }
        bytes += name.len();
    }
    if bytes > 65_536 {
        return Err(RuntimeError::invalid(
            "reload_artifact",
            "remote facet metadata exceeds limit",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Timing {
    gate: super::super::protocol::PositiveId,
    retire: super::super::protocol::PositiveId,
    task_stop: super::super::protocol::PositiveId,
}
impl Timing {
    pub(super) fn from_options(options: &ReloadOptions) -> Result<Self> {
        let milliseconds = |duration: Duration| {
            let milliseconds = u64::try_from(duration.as_millis()).map_err(|_| {
                RuntimeError::invalid("reload_deadline", "reload deadline exceeds wire domain")
            })?;
            if duration != Duration::from_millis(milliseconds) {
                return Err(RuntimeError::invalid(
                    "reload_deadline",
                    "coordinated deadlines require whole milliseconds",
                ));
            }
            super::super::protocol::PositiveId::new(milliseconds)
        };
        Ok(Self {
            gate: milliseconds(options.gate_deadline)?,
            retire: milliseconds(options.retire_deadline)?,
            task_stop: milliseconds(options.task_stop_deadline)?,
        })
    }
    pub(crate) fn options(
        &self,
        remove: Vec<String>,
        signal: crate::CancellationToken,
    ) -> ReloadOptions {
        ReloadOptions {
            remove,
            gate_deadline: Duration::from_millis(self.gate.get()),
            retire_deadline: Duration::from_millis(self.retire.get()),
            task_stop_deadline: Duration::from_millis(self.task_stop.get()),
            cancellation: Some(signal),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Summary {
    pub prepared: super::super::protocol::SafeU64,
    pub gated: super::super::protocol::SafeU64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "status",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum Vote {
    Ready(Summary),
    Rejected(super::super::protocol::WireError),
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn checked_artifacts_and_batches_refuse_before_loader_effects() {
        for (name, artifact) in [("", "id"), ("name", ""), ("a\n", "id")] {
            assert_eq!(
                RemoteFacet::new(name, artifact).unwrap_err().reason,
                "reload_artifact"
            );
        }
        assert!(
            serde_json::from_value::<RemoteFacet>(serde_json::json!({
                "name":"example", "artifact":"ready:1", "path":"forged"
            }))
            .is_err()
        );
        let facet = RemoteFacet::new("example", "ready:1").unwrap();
        assert!(validate_batch(&[facet.clone(), facet.clone()], &[]).is_err());
        assert!(validate_batch(std::slice::from_ref(&facet), &["example".into()]).is_err());
        assert!(validate_batch(&vec![facet; 129], &[]).is_err());
        let batch = (0..128)
            .map(|index| RemoteFacet::new(index.to_string(), "x".repeat(1024)).unwrap())
            .collect::<Vec<_>>();
        assert!(validate_batch(&batch, &[]).is_err());
    }
}
