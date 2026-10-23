//! Bounded, opt-in runtime observations without synchronous subscriber callbacks.
//!
//! Collection uses the Host driver's clock. Its values can be virtual time (ManualDriver),
//! and are neither CPU time nor exclusive execution time. Export only after draining.

mod driver;
mod export;
mod payload;
mod summary;

pub(crate) use driver::ObservedDriver;
pub use export::chrome_trace;
pub use payload::{PayloadCaptureError, capture_payload};
pub use summary::{ObservationSummary, OperationSummary, summarize};

use crate::{Driver, Result, RuntimeError};
use serde::{Deserialize, Serialize};
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    num::NonZeroUsize,
    rc::{Rc, Weak},
    time::Duration,
};

/// Checked collection limits. No payload or principal is collected implicitly.
#[derive(Clone, Deserialize)]
#[serde(try_from = "RawOptions")]
pub struct ObservationOptions {
    capacity: NonZeroUsize,
    sample_every: NonZeroUsize,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOptions {
    capacity: usize,
    sample_every: usize,
}
impl TryFrom<RawOptions> for ObservationOptions {
    type Error = RuntimeError;
    fn try_from(value: RawOptions) -> Result<Self> {
        Self::new(value.capacity, value.sample_every)
    }
}
impl ObservationOptions {
    /// Creates a queue holding at most `capacity` events, sampling every `sample_every` operation.
    ///
    /// # Errors
    /// Refuses zero values and capacities greater than 1,048,576 events. Refusal allocates no queue.
    pub fn new(capacity: usize, sample_every: usize) -> Result<Self> {
        if capacity > 1_048_576 {
            return Err(RuntimeError::invalid(
                "observation_capacity",
                "observation capacity exceeds limit",
            ));
        }
        Ok(Self {
            capacity: NonZeroUsize::new(capacity).ok_or_else(|| {
                RuntimeError::invalid(
                    "observation_capacity",
                    "observation capacity must be positive",
                )
            })?,
            sample_every: NonZeroUsize::new(sample_every).ok_or_else(|| {
                RuntimeError::invalid("observation_sampling", "sampling interval must be positive")
            })?,
        })
    }
}

/// Runtime boundary actually observed; this is not an exhaustive application event taxonomy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Cancellation,
    Acquisition,
    Claim,
    NativeCall,
    PortableCall,
    PhysicalInvocation,
    Root,
    Scope,
    Activation,
    Disposal,
    ResourceOpen,
    ResourceClose,
    StatePublication,
    StateDelivery,
    StateSubscription,
    RegistryRead,
    RegistryPublication,
    Reload,
    RpcSend,
    RpcReceive,
    Hook,
    Executor,
}

/// Outcome of this boundary only. `Settled` makes no statement about external effects.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Ok,
    /// One executor poll suspended; this is not task completion.
    Pending,
    Error {
        code: crate::ErrorCode,
        reason: String,
    },
    /// Its future or owner was dropped before an explicit result was recorded.
    Abandoned,
    /// The actual physical invocation owner released its final lease.
    Settled,
}

/// One real runtime operation. Identifiers are unique within this collection only.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Observation {
    #[serde(with = "decimal")]
    pub id: u64,
    #[serde(with = "optional_decimal")]
    pub parent: Option<u64>,
    #[serde(with = "decimal")]
    pub host: u64,
    #[serde(with = "optional_decimal")]
    pub root: Option<u64>,
    #[serde(with = "optional_decimal")]
    pub object: Option<u64>,
    /// Independent acquisition claim identity, never inferred from object identity.
    #[serde(with = "optional_decimal")]
    pub claim: Option<u64>,
    #[serde(with = "optional_decimal")]
    pub code_version: Option<u64>,
    pub operation: Operation,
    /// UTF-8 label truncated to at most 160 bytes. No principal or argument values.
    pub label: String,
    /// Driver-clock nanoseconds relative to collection creation; absent after Driver destruction.
    #[serde(with = "optional_decimal")]
    pub timestamp_ns: Option<u64>,
    /// `None` starts a span; `Some` ends the same span. Missing pairs can result from overflow.
    pub outcome: Option<Outcome>,
}

/// Cumulative counts include sampled-out operations; event counters distinguish queue loss.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ObservationStatistics {
    pub started: u64,
    pub completed: u64,
    pub abandoned: u64,
    pub sampled_out: u64,
    pub dropped_events: u64,
    pub identifier_exhaustion: u64,
    pub clock_unavailable: u64,
    pub queued: usize,
    pub capacity: usize,
}

struct Inner {
    options: ObservationOptions,
    driver: Weak<dyn Driver>,
    origin: Duration,
    host: u64,
    next: Cell<u64>,
    queue: RefCell<VecDeque<Observation>>,
    statistics: RefCell<ObservationStatistics>,
}

/// Read-only consumer handle; draining changes only diagnostic buffering.
///
/// Collection never invokes user observers, performs I/O, or serializes native values.
/// Allocation failure can still abort the process; custom Driver::now implementations must
/// obey their existing clock contract and must not panic or call back into the runtime.
#[derive(Clone)]
pub struct Observations(Rc<Inner>);
impl Observations {
    pub(crate) fn new(options: ObservationOptions, driver: Rc<dyn Driver>, host: u64) -> Self {
        let origin = driver.now();
        let capacity = options.capacity.get();
        Self(Rc::new(Inner {
            options,
            driver: Rc::downgrade(&driver),
            origin,
            host,
            next: Cell::new(0),
            queue: RefCell::new(VecDeque::with_capacity(capacity)),
            statistics: RefCell::new(ObservationStatistics {
                capacity,
                ..Default::default()
            }),
        }))
    }

    /// Removes at most `limit` oldest events. No runtime callbacks run while draining.
    pub fn drain(&self, limit: usize) -> Vec<Observation> {
        let mut queue = self.0.queue.borrow_mut();
        let count = limit.min(queue.len());
        queue.drain(..count).collect()
    }

    /// Returns cumulative counters and current queue occupancy without resetting either.
    pub fn statistics(&self) -> ObservationStatistics {
        let mut result = self.0.statistics.borrow().clone();
        result.queued = self.0.queue.borrow().len();
        result
    }

    fn push(&self, mut event: Observation) {
        if self.0.queue.borrow().len() == self.0.options.capacity.get() {
            let mut stats = self.0.statistics.borrow_mut();
            stats.dropped_events = stats.dropped_events.saturating_add(1);
            return;
        }
        event.timestamp_ns = self.0.driver.upgrade().map(|driver| {
            driver
                .now()
                .saturating_sub(self.0.origin)
                .as_nanos()
                .min(u64::MAX as u128) as u64
        });
        if event.timestamp_ns.is_none() {
            let mut stats = self.0.statistics.borrow_mut();
            stats.clock_unavailable = stats.clock_unavailable.saturating_add(1);
        }
        self.0.queue.borrow_mut().push_back(event);
    }

    /// Starts an explicit adapter boundary without fabricating Context or object correlation.
    /// It grants no runtime authority and records no values. Dropping records abandonment.
    pub fn observe(&self, operation: Operation, label: &str) -> Option<ObservationSpan> {
        self.start(operation, label, None, None, None)
    }

    pub(crate) fn start(
        &self,
        operation: Operation,
        label: &str,
        context: Option<&crate::Context>,
        object: Option<u64>,
        claim: Option<u64>,
    ) -> Option<ObservationSpan> {
        let mut stats = self.0.statistics.borrow_mut();
        let Some(id) = self.0.next.get().checked_add(1) else {
            stats.identifier_exhaustion = stats.identifier_exhaustion.saturating_add(1);
            return None;
        };
        self.0.next.set(id);
        stats.started = stats.started.saturating_add(1);
        let sampled = (id - 1) % self.0.options.sample_every.get() as u64 == 0;
        if !sampled {
            stats.sampled_out = stats.sampled_out.saturating_add(1);
        }
        drop(stats);
        let event = Observation {
            id,
            parent: context.and_then(crate::Context::observation_id),
            host: self.0.host,
            root: context.map(crate::Context::root_id),
            object,
            claim,
            code_version: context.map(crate::Context::code_version),
            operation,
            label: if sampled {
                bounded(label)
            } else {
                String::new()
            },
            timestamp_ns: None,
            outcome: None,
        };
        if sampled {
            self.push(event.clone());
        }
        Some(ObservationSpan {
            observations: self.clone(),
            event: Some(event),
            sampled,
            id,
        })
    }
}

/// One admitted diagnostic boundary. Dropping it records abandonment, without cancelling work.
/// It retains the collection, never its Driver or a runtime object, Context, or Reference.
pub struct ObservationSpan {
    observations: Observations,
    event: Option<Observation>,
    sampled: bool,
    id: u64,
}
impl ObservationSpan {
    /// Returns this collection-local operation identifier. It is not a runtime capability.
    pub fn id(&self) -> u64 {
        self.id
    }
    /// Records the first explicit boundary outcome; subsequent finishes have no effect.
    pub fn finish(&mut self, mut outcome: Outcome) {
        let Some(mut event) = self.event.take() else {
            return;
        };
        let mut stats = self.observations.0.statistics.borrow_mut();
        stats.completed = stats.completed.saturating_add(1);
        if outcome == Outcome::Abandoned {
            stats.abandoned = stats.abandoned.saturating_add(1);
        }
        drop(stats);
        if let Outcome::Error { reason, .. } = &mut outcome {
            *reason = bounded(reason);
        }
        event.outcome = Some(outcome);
        if self.sampled {
            self.observations.push(event);
        }
    }
    pub(crate) fn acquired(&mut self, claim: Option<u64>, object: u64) {
        if let Some(event) = &mut self.event {
            event.claim = claim;
            event.object = Some(object);
        }
    }

    pub(crate) fn link(&mut self, root: u64, parent: Option<u64>, version: u64) {
        if let Some(event) = &mut self.event {
            event.root = Some(root);
            event.parent = parent;
            event.code_version = Some(version);
        }
    }

    pub(crate) fn context(&mut self, context: &crate::Context) {
        // The start can precede root admission; only the end carries newly admitted facts.
        if let Some(event) = &mut self.event {
            event.root = Some(context.root_id());
            event.parent = context.observation_id();
            event.code_version = Some(context.code_version());
        }
    }
    /// Records only success or the bounded category/reason, without inspecting the value.
    pub fn result<T>(&mut self, result: &Result<T>) {
        self.finish(match result {
            Ok(_) => Outcome::Ok,
            Err(error) => Outcome::Error {
                code: error.code,
                reason: bounded(&error.reason),
            },
        });
    }
}
impl Drop for ObservationSpan {
    fn drop(&mut self) {
        self.finish(Outcome::Abandoned);
    }
}

fn bounded(value: &str) -> String {
    let mut length = value.len().min(160);
    while !value.is_char_boundary(length) {
        length -= 1;
    }
    value[..length].to_owned()
}

pub(crate) type Span = ObservationSpan;

pub(crate) fn start(
    observations: &Option<Observations>,
    operation: Operation,
    label: &str,
    context: Option<&crate::Context>,
    object: Option<u64>,
    claim: Option<u64>,
) -> Option<ObservationSpan> {
    observations
        .as_ref()
        .and_then(|observations| observations.start(operation, label, context, object, claim))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_overflow_sampling_and_abandonment_are_visible() {
        let observations = Observations::new(
            ObservationOptions::new(2, 2).unwrap(),
            crate::ManualDriver::new(),
            1,
        );
        let mut first = observations
            .start(Operation::Root, "root", None, None, None)
            .unwrap();
        first.finish(Outcome::Ok);
        drop(observations.start(Operation::Root, "sampled", None, None, None));
        drop(observations.start(Operation::Root, "overflow", None, None, None));
        let stats = observations.statistics();
        assert_eq!(
            (
                stats.started,
                stats.completed,
                stats.abandoned,
                stats.sampled_out,
                stats.dropped_events,
                stats.queued
            ),
            (3, 3, 2, 1, 2, 2)
        );
        assert_eq!(observations.drain(1).len(), 1);
        assert_eq!(observations.drain(10).len(), 1);
        assert_eq!(observations.statistics().dropped_events, 2);
    }
    #[test]
    fn exhaustion_and_end_after_finish_do_not_mutate_runtime_authority() {
        let driver = crate::ManualDriver::new();
        let observations =
            Observations::new(ObservationOptions::new(8, 1).unwrap(), driver.clone(), 1);
        let mut span = observations.observe(Operation::Root, "root").unwrap();
        let id = span.id();
        span.finish(Outcome::Ok);
        assert_eq!(span.id(), id);
        span.finish(Outcome::Abandoned);
        observations.0.next.set(u64::MAX);
        assert!(observations.observe(Operation::Root, "exhausted").is_none());
        let stats = observations.statistics();
        assert_eq!(stats.identifier_exhaustion, 1);
        assert_eq!(stats.completed, 1);
        assert_eq!(stats.abandoned, 0);
        let events = observations.drain(8);
        let summary = summarize(&events);
        assert_eq!(summary.operations[&Operation::Root].matched, 1);
        assert_eq!(summary.unmatched_starts + summary.unmatched_ends, 0);
        let json = serde_json::to_string(&events).unwrap();
        assert!(json.contains("\"id\":\"1\""));
        let decoded: Vec<Observation> = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded[0].id, 1);
    }

    #[test]
    fn public_completion_bounds_multibyte_failure_reasons() {
        let driver = crate::ManualDriver::new();
        let observations =
            Observations::new(ObservationOptions::new(4, 1).unwrap(), driver.clone(), 1);
        let mut span = observations
            .observe(Operation::Executor, "adapter")
            .unwrap();
        span.finish(Outcome::Error {
            code: crate::ErrorCode::Invalid,
            reason: "界".repeat(100_000),
        });
        let events = observations.drain(4);
        let Some(Outcome::Error { reason, .. }) = &events[1].outcome else {
            panic!("missing error")
        };
        assert!(reason.len() <= 160);
        assert!(reason.chars().all(|value| value == '界'));
    }

    #[test]
    fn refuses_invalid_limits_and_bounds_utf8_labels() {
        assert!(ObservationOptions::new(0, 1).is_err());
        assert!(ObservationOptions::new(1, 0).is_err());
        assert!(ObservationOptions::new(1_048_577, 1).is_err());
        assert!(bounded(&"é".repeat(100)).len() <= 160);
    }
}

mod decimal {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}
mod optional_decimal {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    pub fn serialize<S: Serializer>(value: &Option<u64>, serializer: S) -> Result<S::Ok, S::Error> {
        value.map(|value| value.to_string()).serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<u64>, D::Error> {
        Option::<String>::deserialize(deserializer)?
            .map(|value| value.parse().map_err(serde::de::Error::custom))
            .transpose()
    }
}
