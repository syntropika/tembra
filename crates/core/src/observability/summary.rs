use super::{Observation, Operation, Outcome};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Exact counts and inclusive driver-clock intervals from matched records in one drained batch.
/// No percentiles, CPU estimates, exclusive-time subtraction, or extrapolation from sampling.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OperationSummary {
    pub starts: u64,
    pub ends: u64,
    pub errors: u64,
    pub abandoned: u64,
    pub matched: u64,
    pub inclusive_ns: u64,
    pub max_inclusive_ns: u64,
    pub missing_duration: u64,
}

/// Post-drain aggregation, independent of collection and export adapters.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ObservationSummary {
    /// Keys have fixed cardinality: operation kinds, never arbitrary labels or object IDs.
    pub operations: BTreeMap<Operation, OperationSummary>,
    pub unmatched_starts: usize,
    pub unmatched_ends: usize,
}

/// Summarizes only the supplied batch, using memory bounded by its number of events.
///
/// Cross-batch pairs, sampled-out operations and overflow cannot be reconstructed. Their
/// absence is reported as unmatched records, never estimated duration or a running-work claim.
/// Inclusive intervals overlap and must not be summed as CPU or exclusive elapsed time.
pub fn summarize(events: &[Observation]) -> ObservationSummary {
    let mut summary = ObservationSummary::default();
    let mut starts = BTreeMap::new();
    for event in events {
        let operation = summary.operations.entry(event.operation).or_default();
        if let Some(outcome) = &event.outcome {
            operation.ends += 1;
            operation.errors += u64::from(matches!(outcome, Outcome::Error { .. }));
            operation.abandoned += u64::from(*outcome == Outcome::Abandoned);
            if let Some((kind, timestamp)) = starts.remove(&event.id) {
                if kind == event.operation {
                    operation.matched += 1;
                    if let (Some(end), Some(start)) = (event.timestamp_ns, timestamp) {
                        let duration = end.saturating_sub(start);
                        operation.inclusive_ns = operation.inclusive_ns.saturating_add(duration);
                        operation.max_inclusive_ns = operation.max_inclusive_ns.max(duration);
                    } else {
                        operation.missing_duration += 1;
                    }
                } else {
                    summary.unmatched_ends += 1;
                    summary.unmatched_starts += 1;
                }
            } else {
                summary.unmatched_ends += 1;
            }
        } else {
            operation.starts += 1;
            starts.insert(event.id, (event.operation, event.timestamp_ns));
        }
    }
    summary.unmatched_starts += starts.len();
    summary
}
