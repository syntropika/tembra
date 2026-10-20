import { executorFailure } from "../host/error.js";
import { detached } from "../host/executor.js";
import { assertPortable } from "../portable.js";
import type { JsonValue } from "../portable.js";

/** A retained core background failure or browser callback failure. */
export type BrowserRuntimeError = {
  readonly code: string;
  readonly reason: string;
  readonly message: string;
  readonly details: JsonValue;
};
export type BrowserHostDiagnostics = Readonly<Record<string, unknown>> & {
  readonly errors: readonly BrowserRuntimeError[];
};

/** Checks the exact Rust error envelope while preserving immutable structured details. */
export function browserErrors(text: string): readonly BrowserRuntimeError[] {
  const parsed = detached<unknown>(text);
  assertPortable(parsed);
  if (!Array.isArray(parsed)) {
    throw executorFailure("diagnostics", "browser errors must be an array");
  }
  return Object.freeze(
    parsed.map((entry) => {
      if (
        entry === null ||
        typeof entry !== "object" ||
        Array.isArray(entry) ||
        typeof entry.code !== "string" ||
        typeof entry.reason !== "string" ||
        typeof entry.message !== "string" ||
        entry.details === undefined
      ) {
        throw executorFailure("diagnostics", "browser error envelope is malformed");
      }
      return Object.freeze({
        code: entry.code,
        reason: entry.reason,
        message: entry.message,
        details: entry.details,
      });
    }),
  );
}
