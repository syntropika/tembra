//! Driver-owned local reload orchestration. Pending code is never ordinary call authority.
mod catalog;
mod commit;
mod prepare;
mod retirement;
#[cfg(test)]
mod tests;
mod transaction;
pub(crate) use catalog::CandidateCatalog;
pub(crate) use transaction::PreparedReload;
pub(crate) mod types;
pub(super) use types::ReadyBindings;
pub(crate) use types::{
    CandidateDeclarations, DeclarationSet, DeclarationTarget, InstalledVersion,
};

use crate::CancellationToken;
use std::time::Duration;

/// Administration of one coordinated reload. Deadlines use the Host Driver's clock.
/// Dropping its result observer does not cancel an admitted transaction; the optional
/// token requests abort before the commit decision only.
pub struct ReloadOptions {
    /// Foreign facet names to remove in this same checked transaction.
    pub remove: Vec<String>,
    /// Maximum wait for affected grandfathered roots and physical entries before mutation.
    pub gate_deadline: Duration,
    /// Wait for the existing previous epoch's logical roots before forced cancellation.
    /// Physical work still retains its selected resources until it actually settles.
    pub retire_deadline: Duration,
    /// Cooperative stop wait for replaced tasks on the compatible, ungated path.
    /// Expiry reports a fallback; stopped admission cannot start new roots, but physical
    /// work can overlap the replacement and continues to own its resources.
    pub task_stop_deadline: Duration,
    /// Optional pre-decision abort request. Dropping the result waiter is not cancellation.
    pub cancellation: Option<CancellationToken>,
}
impl ReloadOptions {
    fn validate(&self, now: Duration) -> crate::Result<()> {
        for deadline in [
            self.gate_deadline,
            self.retire_deadline,
            self.task_stop_deadline,
        ] {
            if deadline.is_zero() || now.checked_add(deadline).is_none() {
                return Err(crate::RuntimeError::invalid(
                    "reload_options",
                    "reload deadlines must be positive and fit the Driver clock",
                ));
            }
        }
        Ok(())
    }
}
impl Default for ReloadOptions {
    fn default() -> Self {
        Self {
            remove: Vec::new(),
            gate_deadline: Duration::from_secs(5),
            retire_deadline: Duration::from_secs(5),
            task_stop_deadline: Duration::from_secs(5),
            cancellation: None,
        }
    }
}
/// Observations of a completed reload, without version-selection authority.
#[derive(Clone, Debug)]
pub struct ReloadReport {
    pub(crate) current: u64,
    pub(crate) previous: Option<u64>,
    pub(crate) changed: Vec<String>,
    pub(crate) removed: Vec<String>,
    pub(crate) prepared: Vec<u64>,
    pub(crate) gated: Vec<u64>,
    pub(crate) lingering: usize,
}
impl ReloadReport {
    pub fn current_version(&self) -> u64 {
        self.current
    }
    /// The callable previous version observed at completion; this grants no authority.
    pub fn previous_version(&self) -> Option<u64> {
        self.previous
    }
    pub fn changed(&self) -> &[String] {
        &self.changed
    }
    pub fn removed(&self) -> &[String] {
        &self.removed
    }
    pub fn prepared_objects(&self) -> &[u64] {
        &self.prepared
    }
    pub fn gated_objects(&self) -> &[u64] {
        &self.gated
    }
    pub fn lingering_physical(&self) -> usize {
        self.lingering
    }
}

/// Physical lifetime of the one driver-owned transaction; Host close joins it.
pub(super) struct ReloadControl {
    pub(super) cancellation: CancellationToken,
    pub(super) settled: CancellationToken,
}
