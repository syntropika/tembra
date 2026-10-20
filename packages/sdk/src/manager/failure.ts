/** Preserve the core's structured connection refusal across platform-specific wire representations. */
export function connectionFailure(value: unknown): Error {
  if (value instanceof Error) {
    return value;
  }
  let cause = value;
  if (typeof cause === "string") {
    try {
      cause = JSON.parse(cause);
    } catch {
      // A custom transport may report diagnostic text rather than the core error envelope.
    }
  }
  if (
    cause !== null &&
    typeof cause === "object" &&
    "message" in cause &&
    typeof cause.message === "string"
  ) {
    const error = new Error(cause.message, { cause });
    if ("code" in cause && typeof cause.code === "string") {
      Object.assign(error, { code: cause.code });
    }
    if ("reason" in cause && typeof cause.reason === "string") {
      Object.assign(error, { reason: cause.reason });
    }
    if ("details" in cause) {
      Object.assign(error, { details: cause.details, detailsJson: JSON.stringify(cause.details) });
    } else if ("detailsJson" in cause && typeof cause.detailsJson === "string") {
      Object.assign(error, { detailsJson: cause.detailsJson });
      try {
        const details: unknown = JSON.parse(cause.detailsJson);
        Object.assign(error, { details });
      } catch {
        // Retain malformed custom-transport details as text without replacing the causal failure.
      }
    }
    return error;
  }
  return new Error("managed RPC connection closed", { cause });
}
export function lifecycleFailure(error: unknown): Error {
  return error instanceof Error ? error : new Error(String(error), { cause: error });
}

/** Control failures carry portable core diagnostic fields, never arbitrary JavaScript captures. */
export function failureMessage(value: unknown): {
  type: "failure";
  message: string;
  code?: string;
  reason?: string;
  detailsJson?: string;
} {
  const error = lifecycleFailure(value);
  const message: {
    type: "failure";
    message: string;
    code?: string;
    reason?: string;
    detailsJson?: string;
  } = {
    type: "failure",
    message: error.message,
  };
  if ("code" in error && typeof error.code === "string") {
    message.code = error.code;
  }
  if ("reason" in error && typeof error.reason === "string") {
    message.reason = error.reason;
  }
  if ("detailsJson" in error && typeof error.detailsJson === "string") {
    message.detailsJson = error.detailsJson;
  }
  return message;
}
