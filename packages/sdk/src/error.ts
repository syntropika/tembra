import { Errors, type ErrorDescriptor } from "./errors.ts";
import type { Code } from "./identifiers.ts";

/** A native JavaScript Error carrying the same structured failure fields as Rust. */
export class TembraError extends Error {
  readonly code: Code;
  readonly reason: string;
  constructor(error: ErrorDescriptor, message = error.message, options?: ErrorOptions) {
    super(message, options);
    this.name = "TembraError";
    this.code = error.code;
    this.reason = error.reason;
  }
}

export function failure(error: unknown): TembraError {
  if (error instanceof TembraError) return error;
  return new TembraError(
    Errors.Unavailable.Callback,
    error instanceof Error ? error.message : String(error),
    { cause: error },
  );
}

export function refuse(error: ErrorDescriptor, message = error.message): never {
  throw new TembraError(error, message);
}
