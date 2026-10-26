import { Code } from "./bridge/identifiers.ts";
import { Errors, type ErrorDescriptor } from "./errors.ts";

const codes: ReadonlySet<unknown> = new Set(Object.values(Code));

/** Check for a structured error descriptor. */
function isDescriptor(value: unknown): value is ErrorDescriptor {
  return (
    typeof value === "object" &&
    value !== null &&
    "code" in value &&
    codes.has(value.code) &&
    "reason" in value &&
    typeof value.reason === "string" &&
    "message" in value &&
    typeof value.message === "string"
  );
}

/** A native JavaScript Error carrying the same structured failure fields as Rust. */
export class TembraError extends Error {
  readonly code: Code;
  readonly reason: string;

  /** Preserve the error code and reason with a readable message. */
  constructor(error: ErrorDescriptor, message = error.message, options?: ErrorOptions) {
    super(message, options);
    this.name = "TembraError";
    this.code = error.code;
    this.reason = error.reason;
  }
}

/** Normalizes failures from other bundles or realms without relying on their constructors. */
export function failure(error: unknown): TembraError {
  let message = Errors.Unavailable.Callback.message;
  try {
    if (error instanceof TembraError) return error;
    if (isDescriptor(error)) return new TembraError(error, error.message, { cause: error });
    message = String(error instanceof Error ? error.message : error);
  } catch {
    // Arbitrary thrown values must not interrupt error reporting or effect acknowledgement.
  }
  return new TembraError(Errors.Unavailable.Callback, message, { cause: error });
}

/** Throw a structured error for a rejected operation. */
export function refuse(error: ErrorDescriptor, message = error.message): never {
  throw new TembraError(error, message);
}
