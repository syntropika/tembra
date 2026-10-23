/** Constructor-only bounded core metadata collection. Sampling is per operation. */
export type ObservationOptions = { readonly capacity: number; readonly sample_every: number };
const operations = [
  "cancellation",
  "acquisition",
  "claim",
  "native_call",
  "portable_call",
  "physical_invocation",
  "root",
  "scope",
  "activation",
  "disposal",
  "resource_open",
  "resource_close",
  "state_publication",
  "state_delivery",
  "state_subscription",
  "registry_read",
  "registry_publication",
  "reload",
  "rpc_send",
  "rpc_receive",
  "hook",
  "executor",
] as const;
export type ObservationOperation = (typeof operations)[number];
/** Decimal IDs and timestamps preserve the core's complete integer precision. */
export type Observation = {
  readonly id: string;
  readonly parent: string | null;
  readonly host: string;
  readonly root: string | null;
  readonly object: string | null;
  readonly claim: string | null;
  readonly code_version: string | null;
  readonly operation: ObservationOperation;
  readonly label: string;
  readonly timestamp_ns: string | null;
  readonly outcome:
    | null
    | "ok"
    | "pending"
    | "abandoned"
    | "settled"
    | { readonly error: { readonly code: string; readonly reason: string } };
};
export type ObservationStatistics = {
  readonly started: number;
  readonly completed: number;
  readonly abandoned: number;
  readonly sampled_out: number;
  readonly dropped_events: number;
  readonly identifier_exhaustion: number;
  readonly clock_unavailable: number;
  readonly queued: number;
  readonly capacity: number;
};
export type ObservationBatch = {
  readonly events: readonly Observation[];
  readonly statistics: ObservationStatistics;
};

/** Shared checked drain bound before calling either native ABI. */
export function checkObservationLimit(limit: number): void {
  if (!Number.isInteger(limit) || limit < 0 || limit > 4_294_967_295) {
    throw new RangeError("Observation limit must be a uint32");
  }
}

/** A bounded final snapshot remains plain data after the runtime has been disposed. */
export function drainFinalObservations(
  batch: ObservationBatch | null,
  limit: number,
): {
  readonly result: ObservationBatch | null;
  readonly remaining: ObservationBatch | null;
} {
  if (batch === null) {
    return { result: null, remaining: null };
  }
  const events = batch.events.slice(0, limit);
  const remaining = batch.events.slice(limit);
  const statistics = { ...batch.statistics, queued: remaining.length };
  return {
    result: { events, statistics },
    remaining: { events: remaining, statistics },
  };
}

function record(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
function invalid(): never {
  throw new TypeError("Invalid core observation response");
}
function decimal(value: unknown): string {
  if (
    typeof value !== "string" ||
    value.length > 20 ||
    !/^(?:0|[1-9]\d*)$/u.test(value) ||
    BigInt(value) > 18_446_744_073_709_551_615n
  ) {
    return invalid();
  }
  return value;
}
function optionalDecimal(value: unknown): string | null {
  return value === null ? null : decimal(value);
}
function count(value: unknown): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0) {
    return invalid();
  }
  return value;
}
function text(value: unknown): string {
  if (typeof value !== "string") {
    return invalid();
  }
  return value;
}
function operation(value: unknown): ObservationOperation {
  for (const candidate of operations) {
    if (candidate === value) {
      return candidate;
    }
  }
  return invalid();
}
function outcome(value: unknown): Observation["outcome"] {
  if (
    value === null ||
    value === "ok" ||
    value === "pending" ||
    value === "abandoned" ||
    value === "settled"
  ) {
    return value;
  }
  if (!record(value) || !record(value.error)) {
    return invalid();
  }
  return { error: { code: text(value.error.code), reason: text(value.error.reason) } };
}
function event(value: unknown): Observation {
  if (!record(value)) {
    return invalid();
  }
  return {
    id: decimal(value.id),
    parent: optionalDecimal(value.parent),
    host: decimal(value.host),
    root: optionalDecimal(value.root),
    object: optionalDecimal(value.object),
    claim: optionalDecimal(value.claim),
    code_version: optionalDecimal(value.code_version),
    operation: operation(value.operation),
    label: text(value.label),
    timestamp_ns: optionalDecimal(value.timestamp_ns),
    outcome: outcome(value.outcome),
  };
}

/** Validates the native/WASM metadata response before exposing its typed public shape. */
export function decodeObservationBatch(value: unknown): ObservationBatch | null {
  if (value === null) {
    return null;
  }
  if (!record(value) || !Array.isArray(value.events) || !record(value.statistics)) {
    return invalid();
  }
  const { statistics } = value;
  const capacity = count(statistics.capacity);
  if (capacity < 1 || capacity > 1_048_576 || value.events.length > capacity) {
    return invalid();
  }
  const events: unknown[] = value.events;
  return {
    events: events.map(event),
    statistics: {
      started: count(statistics.started),
      completed: count(statistics.completed),
      abandoned: count(statistics.abandoned),
      sampled_out: count(statistics.sampled_out),
      dropped_events: count(statistics.dropped_events),
      identifier_exhaustion: count(statistics.identifier_exhaustion),
      clock_unavailable: count(statistics.clock_unavailable),
      queued: count(statistics.queued),
      capacity,
    },
  };
}
/** JSON decoding is kept outside the core and never feeds data back into an invocation. */
export function parseObservationBatch(value: string): ObservationBatch | null {
  const decoded: unknown = JSON.parse(value);
  return decodeObservationBatch(decoded);
}
