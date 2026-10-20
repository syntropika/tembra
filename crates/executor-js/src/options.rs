use tembra_core::{Result, RuntimeError};

/// Positive admission limits for one local engine, checked by [`crate::Executor::new`].
///
/// Limits apply independently of core code versions and are shared by Hosts using the engine.
/// Refusal does not undo earlier callback or declaration effects. They bound admitted slots and
/// work, not arbitrary JavaScript heap allocation or the lifetime of uncooperative promises.
/// Releasing callbacks does not evict evaluated modules or reset cumulative source admission.
#[derive(Clone, Debug)]
pub struct ExecutorOptions {
    /// Maximum UTF-8 bytes in one artifact. Oversize source is refused before queue admission.
    pub max_source_bytes: usize,
    /// Cumulative evaluation admissions, including later syntax/evaluation failures.
    /// Exhaustion refuses preparation with `unavailable/rotation_required`; no module is evicted.
    pub max_evaluations: usize,
    /// Cumulative UTF-8 source bytes charged at evaluation admission, including later failures.
    /// Preflight refusal is not an evaluation; dropping its waiter does not refund admitted work.
    pub max_total_source_bytes: usize,
    /// Simultaneously retained checked callback slots. This is not a count of all JS functions.
    pub max_callbacks: usize,
    /// Simultaneously retained issued binding slots, including Context and native-value handles.
    /// Reusing an existing checked slot does not charge a new slot; dead weak entries are pruned.
    pub max_capabilities: usize,
    /// Simultaneously admitted callback/native-operation jobs through settlement and conversion.
    /// Cancellation or observer abandonment does not return credit before physical completion.
    /// Module evaluations have their own cumulative budget and are not counted by this limit.
    pub max_physical_jobs: usize,
    /// Admitted requests awaiting pump dispatch. Physical work has a separate limit; an empty
    /// queue does not mean the engine is idle. Callable refusal reports its admission phase.
    pub max_queued_jobs: usize,
    /// Simultaneously retained issued Context slots, also included in `max_capabilities`.
    /// This does not count core Contexts that were never issued to JavaScript.
    pub max_contexts: usize,
}
impl Default for ExecutorOptions {
    fn default() -> Self {
        Self {
            max_source_bytes: 1_048_576,
            max_evaluations: 32,
            max_total_source_bytes: 8_388_608,
            max_callbacks: 1024,
            max_capabilities: 1024,
            max_physical_jobs: 128,
            max_queued_jobs: 64,
            max_contexts: 128,
        }
    }
}
impl ExecutorOptions {
    pub(crate) fn check(&self) -> Result<()> {
        if [
            self.max_source_bytes,
            self.max_evaluations,
            self.max_total_source_bytes,
            self.max_callbacks,
            self.max_capabilities,
            self.max_physical_jobs,
            self.max_queued_jobs,
            self.max_contexts,
        ]
        .contains(&0)
        {
            return Err(RuntimeError::invalid(
                "executor_options",
                "executor limits must be positive",
            ));
        }
        Ok(())
    }
}

/// An instantaneous observation of retained slots/work and cumulative evaluation admission.
///
/// These counters do not create ownership or wait for quiescence. Zero live counters do not
/// establish JavaScript heap collection or ESM cache eviction. Retained application definitions
/// and admitted uncooperative work may legitimately keep slots or the engine alive.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExecutorStats {
    /// Cumulative evaluation admissions; failures after admission remain charged.
    pub evaluations: usize,
    /// Cumulative admitted UTF-8 source bytes; callback retirement does not reduce this value.
    pub source_bytes: usize,
    /// Currently retained checked callback slots, excluding unrelated JS functions.
    pub callbacks: usize,
    /// Currently live issued binding slots; expired weak table entries are excluded.
    pub capabilities: usize,
    /// Currently retained issued Context slots, a subset of `capabilities`.
    pub contexts: usize,
    /// Admitted requests still waiting for pump dispatch, excluding started physical work.
    pub queued_jobs: usize,
    /// Admitted callback/native-operation jobs not yet physically settled and converted.
    /// Pending module evaluations are accounted by `evaluations`, not this live counter.
    pub physical_jobs: usize,
}
