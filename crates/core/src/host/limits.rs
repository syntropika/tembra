use crate::{Result, RuntimeError};

/// Host-wide RPC admission limits. Install through `HostOptions::with_rpc_limits`.
/// Per-connection limits are additional restrictions; neither can raise the other.
#[derive(Clone, Debug)]
pub struct HostRpcLimits {
    pub max_connections: usize,
    /// Outgoing consumer bindings across this Host, independent of incoming served holds.
    /// Incoming claims are bounded by connections times each connection's hold limit.
    pub max_remote_holds: usize,
    pub max_pending_routes: usize,
    pub max_incoming_calls: usize,
    pub max_staging_bytes: usize,
    pub max_imported_roots: usize,
    pub max_ended_root_history: usize,
    pub max_revocation_history: usize,
    pub max_revocation_bytes: usize,
}
impl Default for HostRpcLimits {
    fn default() -> Self {
        Self {
            max_connections: 64,
            max_remote_holds: 1024,
            max_pending_routes: 128,
            max_incoming_calls: 256,
            max_staging_bytes: 8_388_608,
            max_imported_roots: 256,
            max_ended_root_history: 16_384,
            max_revocation_history: 16_384,
            max_revocation_bytes: 4_194_304,
        }
    }
}
impl HostRpcLimits {
    pub(super) fn validate(&self) -> Result<()> {
        if [
            self.max_connections,
            self.max_remote_holds,
            self.max_pending_routes,
            self.max_incoming_calls,
            self.max_staging_bytes,
            self.max_imported_roots,
            self.max_ended_root_history,
            self.max_revocation_history,
            self.max_revocation_bytes,
        ]
        .into_iter()
        .any(|value| value == 0 || value as u64 > 9_007_199_254_740_991)
        {
            return Err(RuntimeError::invalid(
                "rpc_limits",
                "Host RPC limits must be positive safe integers",
            ));
        }
        Ok(())
    }
}
