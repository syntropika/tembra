/**
 * Run a test's cleanup without hiding the test's own failure. A cleanup failure after a test
 * failure is reported together with it as an `AggregateError`; nested aggregate messages are
 * flattened so the inner object/host cleanup causes are visible in the report.
 */
export async function cleanup(failure: unknown, close: () => unknown): Promise<void> {
  try {
    await close();
  } catch (closing) {
    const describe = (error: unknown): string =>
      error instanceof AggregateError
        ? `${error.message} [${error.errors.map(describe).join("; ")}]`
        : String((error as Error)?.message ?? error);
    if (failure === undefined) throw new AggregateError([closing], describe(closing));
    throw new AggregateError(
      [failure, closing],
      `${describe(failure)} (cleanup also failed: ${describe(closing)})`,
    );
  }
}

/** Every leaf error of a (nested) cleanup `AggregateError`. */
export function leaves(error: unknown): unknown[] {
  return error instanceof AggregateError ? error.errors.flatMap(leaves) : [error];
}

/**
 * Close a host whose strict cases deliberately quarantined revisions that still held source
 * resources. A quarantined revision cannot run its resource close callbacks, so the host reports
 * that cleanup honestly. Require exactly that: the close fails, and every inner cause is a
 * quarantined revision. Any other cleanup failure is rethrown.
 */
export async function closeQuarantined(close: () => unknown): Promise<void> {
  let closing: unknown;
  try {
    await close();
  } catch (error) {
    closing = error;
  }
  if (closing === undefined) throw new Error("expected the quarantined revisions' cleanup to fail");
  const unexpected = leaves(closing).filter(
    (error) => !/quarantined/.test(String((error as Error)?.message ?? error)),
  );
  if (unexpected.length)
    throw new AggregateError(unexpected, "host cleanup failed for a non-quarantined reason");
}
