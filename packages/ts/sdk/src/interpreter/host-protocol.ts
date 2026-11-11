/**
 * Private host-operation protocol between the interpreter guest and the SDK host.
 * Mirrors `crates/executor-wasm/src/protocol.rs`: unknown operation names are unsupported,
 * while a known operation with missing, mistyped or extra fields is malformed.
 * Payloads (`initial`, `value`, `args`) stay JSON because the WIT contract validates them.
 * Snapshot reads may name the revision the guest already retains (`known`); the host then
 * answers with the current identity and omits a value that has not changed.
 */

/** Handle issued by one environment table; `scope` selects the table, never an authority. */
export interface Handle {
  readonly scope?: number;
  readonly id: number;
}

/** A snapshot read optionally names the kernel revision whose value the guest already holds. */
export interface SnapshotRead extends Handle {
  readonly known?: string;
}

/** Longest kernel revision spelling accepted from a guest (a decimal `u64`). */
const REVISION_LENGTH = 20;

export type LogLevel = "log" | "debug" | "info" | "warn" | "error";

/** Synchronous host operations. */
export type HostRequest =
  | { readonly operation: "console"; readonly level: LogLevel; readonly message: string }
  | { readonly operation: "address" }
  | { readonly operation: "orb" }
  | { readonly operation: "signal.cancelled" }
  | { readonly operation: "context.check" }
  | { readonly operation: "context.principal" }
  | { readonly operation: "context.cancel" }
  | {
      readonly operation: "state.declare";
      readonly initial: unknown;
      readonly version: number;
      readonly validate: number | null;
    }
  | ({ readonly operation: "state.read" } & SnapshotRead)
  | ({ readonly operation: "state.update"; readonly value: unknown } & Handle)
  | {
      readonly operation: "reference.declare";
      readonly service: string;
      readonly key: readonly string[];
    }
  | ({ readonly operation: "reference.release" } & Handle)
  | ({ readonly operation: "reference.status" } & Handle)
  | ({ readonly operation: "reference.state" } & SnapshotRead)
  | {
      readonly operation: "resource.declare";
      readonly name: string;
      readonly version: number;
      readonly open: number;
      readonly close: number;
      readonly dependencies: readonly Handle[];
    }
  | ({ readonly operation: "resource.read" } & Handle)
  | { readonly operation: "hook.declare"; readonly name: string };

/** Asynchronous host operations, awaited outside the guest heap. */
export type AsyncHostRequest =
  | ({ readonly operation: "hook.run"; readonly value: unknown } & Handle)
  | ({ readonly operation: "reference.ready" } & Handle)
  /**
   * `Context.get`: the guest names only a selected import binding and key; the host supplies the
   * invocation, principal and requirement. Source handles use their environment's table;
   * checked compiled reference views use their Orb table, shared by the Store's environments.
   */
  | {
      readonly operation: "reference.acquire";
      readonly service: string;
      readonly key: readonly string[];
    }
  | ({
      readonly operation: "reference.call";
      readonly method: string;
      readonly args: unknown;
    } & Handle)
  | { readonly operation: "wait"; readonly milliseconds: number };

/** Longest cooperative timer a callback may request. */
export const WAIT_LIMIT_MILLISECONDS = 5000;

/** Field kinds; a trailing `?` marks a field that may be absent (or null) and takes a default. */
type Kind =
  | "u32"
  | "string"
  | "keys"
  | "level"
  | "json"
  | "json?"
  | "callback?"
  | "scope?"
  | "revision?"
  | "handles?";
type Schema = Readonly<Record<string, Kind>>;

const HANDLE = { scope: "scope?", id: "u32" } as const satisfies Schema;
const SNAPSHOT = { ...HANDLE, known: "revision?" } as const satisfies Schema;
const SYNC = {
  console: { level: "level", message: "string" },
  address: {},
  orb: {},
  "signal.cancelled": {},
  "context.check": {},
  "context.principal": {},
  "context.cancel": {},
  "state.declare": { initial: "json?", version: "u32", validate: "callback?" },
  "state.read": SNAPSHOT,
  "state.update": { ...HANDLE, value: "json?" },
  "reference.declare": { service: "string", key: "keys" },
  "reference.release": HANDLE,
  "reference.status": HANDLE,
  "reference.state": SNAPSHOT,
  "resource.declare": {
    name: "string",
    version: "u32",
    open: "u32",
    close: "u32",
    dependencies: "handles?",
  },
  "resource.read": HANDLE,
  "hook.declare": { name: "string" },
} as const satisfies Record<HostRequest["operation"], Schema>;
const ASYNC = {
  "hook.run": { ...HANDLE, value: "json" },
  "reference.ready": HANDLE,
  "reference.acquire": { service: "string", key: "keys" },
  "reference.call": { ...HANDLE, method: "string", args: "json" },
  wait: { milliseconds: "u32" },
} as const satisfies Record<AsyncHostRequest["operation"], Schema>;
const LEVELS: readonly string[] = ["log", "debug", "info", "warn", "error"];

/** Decode one synchronous packet into a typed request. */
export function decodeHostRequest(packet: string): HostRequest {
  return decode(packet, SYNC, "host request") as HostRequest;
}

/** Decode one asynchronous packet into a typed request. */
export function decodeAsyncHostRequest(packet: string): AsyncHostRequest {
  return decode(packet, ASYNC, "asynchronous host request") as AsyncHostRequest;
}

/** Diagnostics and reads of already issued values stay usable after cancellation. */
export function survivesCancellation(request: HostRequest): boolean {
  switch (request.operation) {
    case "console":
    case "signal.cancelled":
    case "state.read":
    case "resource.read":
    case "reference.state":
    case "reference.status":
    case "reference.release":
      return true;
    default:
      return false;
  }
}

/** Validate a packet against the schema for its operation, filling defaults for optional fields. */
function decode(packet: string, schemas: Readonly<Record<string, Schema>>, label: string): unknown {
  const malformed = (reason: string) => new Error(`Malformed ${label}: ${reason}`);
  let raw: unknown;
  try {
    raw = JSON.parse(packet);
  } catch {
    throw malformed("invalid JSON");
  }
  if (!isRecord(raw)) throw malformed("expected an object");
  const operation = raw.operation;
  if (typeof operation !== "string") throw malformed("expected string operation");
  if (!Object.hasOwn(schemas, operation))
    throw new Error(`Unsupported ${label} operation ${operation}`);
  const schema = schemas[operation]!;
  for (const key of Object.keys(raw))
    if (key !== "operation" && !Object.hasOwn(schema, key)) throw malformed(`unknown field ${key}`);
  const request: Record<string, unknown> = { operation };
  for (const [key, kind] of Object.entries(schema)) {
    const value = field(raw[key], kind, () => malformed(`invalid field ${key}`));
    if (value !== undefined) request[key] = value;
  }
  return request;
}

/** Check one field and return its normalized value; `undefined` means omitted. */
function field(value: unknown, kind: Kind, invalid: () => Error): unknown {
  const absent = value === undefined || value === null;
  switch (kind) {
    case "u32":
      if (!isU32(value)) throw invalid();
      return value;
    case "string":
      if (typeof value !== "string") throw invalid();
      return value;
    case "keys":
      if (!Array.isArray(value) || !value.every((item) => typeof item === "string"))
        throw invalid();
      return value;
    case "level":
      if (typeof value !== "string" || !LEVELS.includes(value)) throw invalid();
      return value;
    case "json":
      if (value === undefined) throw invalid();
      return value;
    case "json?":
      return value ?? null;
    case "callback?":
      if (absent) return null;
      if (!isU32(value)) throw invalid();
      return value;
    case "revision?":
      if (absent) return undefined;
      if (typeof value !== "string" || value.length > REVISION_LENGTH || !/^\d+$/.test(value))
        throw invalid();
      return value;
    case "scope?":
      if (absent) return undefined;
      if (!Number.isSafeInteger(value) || (value as number) < 0) throw invalid();
      return value;
    case "handles?":
      if (absent) return [];
      if (!Array.isArray(value)) throw invalid();
      return value.map((item) => {
        if (!isRecord(item) || Object.keys(item).some((key) => key !== "scope" && key !== "id"))
          throw invalid();
        const scope = field(item.scope, "scope?", invalid);
        return scope === undefined
          ? { id: field(item.id, "u32", invalid) }
          : { scope, id: field(item.id, "u32", invalid) };
      });
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isU32(value: unknown): value is number {
  return Number.isInteger(value) && (value as number) >= 0 && (value as number) <= 0xffff_ffff;
}
