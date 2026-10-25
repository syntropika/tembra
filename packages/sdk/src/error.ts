export class TembraError extends Error {
  readonly code: string;
  readonly reason: string;
  constructor(code: string, reason: string, message = reason, options?: ErrorOptions) {
    super(message, options);
    this.name = "TembraError";
    this.code = code;
    this.reason = reason;
  }
}

export function failure(error: unknown): TembraError {
  if (error instanceof TembraError) return error;
  return new TembraError(
    "unavailable",
    "callback",
    error instanceof Error ? error.message : String(error),
    { cause: error },
  );
}

export function refuse(reason: string, message = reason): never {
  throw new TembraError("invalid", reason, message);
}
