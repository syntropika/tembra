/** Fails a portable conformance observation without depending on a test runner. */
export function check(value: unknown, message: string): asserts value {
  if (value === undefined || value === null || value === false) {
    throw new Error(message);
  }
}

/** Controls physical callback settlement independently from logical cancellation. */
export function deferredVoid(): { promise: Promise<void>; resolve: () => void } {
  let resolveGate!: () => void;
  // oxlint-disable-next-line promise/avoid-new -- The caller explicitly owns the physical settlement gate.
  const promise = new Promise<void>((resolve) => {
    resolveGate = resolve;
  });
  return { promise, resolve: resolveGate };
}

/** Requires an observation produced by the real runtime under test. */
export function defined<T>(value: T | undefined): T {
  if (value === undefined) {
    throw new Error("expected a defined test observation");
  }
  return value;
}

/** Matches a structured runtime refusal independently of its platform error subclass. */
export function reason(expected: string): (error: unknown) => boolean {
  return (error) => error instanceof Error && "reason" in error && error.reason === expected;
}

export function structuredError(error: unknown): asserts error is Error & {
  reason: string;
  detailsJson: string;
  details: Record<string, unknown>;
} {
  check(error instanceof Error, "expected a runtime Error");
  check("reason" in error && typeof error.reason === "string", "expected a structured reason");
  check(
    "detailsJson" in error && typeof error.detailsJson === "string",
    "expected JSON diagnostics",
  );
  check(
    "details" in error && error.details !== null && typeof error.details === "object",
    "expected structured diagnostic details",
  );
}

/**
 * Checks selected managed ownership counters after the caller has joined cleanup.
 * This observes managed quiescence, not physical JavaScript module unloading or heap size.
 */
export function assertQuiescent(counters: Readonly<Record<string, number>>): void {
  const outstanding = Object.entries(counters).filter(([, value]) => value !== 0);
  check(
    outstanding.length === 0,
    `managed work remains: ${outstanding.map(([name, value]) => `${name}=${value}`).join(", ")}`,
  );
}
