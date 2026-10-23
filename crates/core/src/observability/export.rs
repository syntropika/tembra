use super::Observation;

/// Converts already-drained metadata into Chrome Trace events, without touching the runtime.
///
/// Async begin/end pairs use operation IDs. Durations include suspension and overlapping work;
/// they must not be summed as exclusive time. Queue overflow can leave unmatched pairs. Events with no live clock are omitted.
pub fn chrome_trace(events: &[Observation]) -> serde_json::Value {
    serde_json::json!({ "omitted_missing_clock": events.iter().filter(|event| event.timestamp_ns.is_none()).count(), "traceEvents": events.iter().filter(|event| event.timestamp_ns.is_some()).map(|event| serde_json::json!({
        "name": format!("{:?}:{}", event.operation, event.label), "cat": "tembra", "id": event.id.to_string(),
        "ph": if event.outcome.is_some() { "e" } else { "b" }, "pid": event.host, "tid": 0,
        "ts": event.timestamp_ns.unwrap_or_default() as f64 / 1000.0, "args": event,
    })).collect::<Vec<_>>() })
}
