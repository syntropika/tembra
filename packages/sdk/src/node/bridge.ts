import { createRequire } from "node:module";
import { arch, platform } from "node:process";
import { fileURLToPath } from "node:url";

import type { ObservationOptions } from "../host/observability.js";
import { canonicalJson } from "../portable.js";

/** Private same-thread Node-API transport; it carries capabilities, never runtime state. */
export type NativeBridge = {
  request: (operation: string, handle: string, args: string) => string;
  poll: () => string;
  complete: (job: string, result: string) => void;
};
type NativeModule = {
  loaderAuthority: () => string;
  NodeBridge: new (
    name: string,
    options: string,
    dispatch: (request: string) => string,
  ) => NativeBridge;
};
type Wire = null | boolean | number | string | Wire[] | { [key: string]: Wire };
type RecordValue = Record<string, unknown>;

function isMessages(value: unknown): value is unknown[] {
  return Array.isArray(value);
}

function fields(value: unknown): RecordValue {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    throw new Error("invalid native bridge envelope");
  }
  // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- This private wire boundary decodes the locally built Rust ABI after its shape checks.
  return value as RecordValue;
}
function failure(value: unknown): Error {
  const data = fields(value);
  return Object.assign(
    new Error(typeof data.message === "string" ? data.message : "native operation failed"),
    data,
    {
      detailsJson: JSON.stringify(data.details ?? null),
    },
  );
}
function rejection(value: unknown): Wire {
  try {
    // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- This private wire boundary decodes the locally built Rust ABI after its shape checks.
    const data = value !== null && typeof value === "object" ? (value as RecordValue) : {};
    let details: Wire = null;
    try {
      /* oxlint-disable typescript/no-unsafe-type-assertion -- Error details come from parsed detached JSON. */
      details = JSON.parse(
        typeof data.detailsJson === "string" ? data.detailsJson : "null",
      ) as Wire;
      /* oxlint-enable typescript/no-unsafe-type-assertion */
    } catch {
      /* Error details are optional observations. */
    }
    return {
      code: typeof data.code === "string" ? data.code : "invalid",
      reason: typeof data.reason === "string" ? data.reason : "javascript",
      message: typeof data.message === "string" ? data.message : String(value),
      details,
    };
  } catch {
    // Thrown values can have hostile getters or coercions; normalization must still settle Rust.
    return {
      code: "invalid",
      reason: "javascript",
      message: "JavaScript callback failed",
      details: null,
    };
  }
}

function portableRead(input: unknown): () => string {
  return () =>
    canonicalJson(input === undefined ? { present: false } : { present: true, value: input });
}

export function nativeTransport(
  name: string,
  binary?: string | URL,
  observability?: ObservationOptions,
) {
  const require = createRequire(import.meta.url);
  const filename =
    binary ?? new URL(`../../native/tembra.${platform}-${arch}.node`, import.meta.url);
  /* oxlint-disable typescript/no-unsafe-type-assertion -- The packaged Node-API addon implements this private ABI. */
  const module = require(
    filename instanceof URL ? fileURLToPath(filename) : filename,
  ) as NativeModule;
  /* oxlint-enable typescript/no-unsafe-type-assertion */
  const authority = module.loaderAuthority();
  const authorityKey = Symbol.for("@tembra/sdk/node.loader-authority.v1");
  const prior = Object.getOwnPropertyDescriptor(globalThis, authorityKey);
  if (prior !== undefined && prior.value !== authority) {
    throw Object.assign(
      new Error("multiple Node addon loader authorities in one JavaScript environment"),
      {
        code: "invalid",
        reason: "loader_authority",
      },
    );
  }
  if (prior === undefined) {
    Object.defineProperty(globalThis, authorityKey, { value: authority });
  }
  const callbacks = new Map<string, (...args: unknown[]) => unknown>();
  const values = new Map<string, unknown>();
  const eventSources = new WeakMap<object, string>();
  const handles = new WeakMap<object, { id: string; kind: string }>();
  const issued = new Map<string, WeakRef<object>>();
  const jobs = new Map<
    string,
    { resolve: (value: unknown) => void; reject: (reason: unknown) => void }
  >();
  const allocations: { callbacks: Set<string>; values: Set<string> }[] = [];
  let serial = 0;
  let disposed = false;
  let polling = false;
  const native = new module.NodeBridge(
    name,
    canonicalJson(observability === undefined ? {} : { observability }),
    dispatch,
  );
  const finalized = new FinalizationRegistry<string>((id) => {
    issued.delete(id);
    if (!disposed) {
      native.request("free", id, "[]");
    }
  });
  const identity = () => {
    serial += 1;
    return String(serial);
  };
  const retain = (value: unknown): Wire => {
    const id = identity();
    values.set(id, value);
    allocations.at(-1)?.values.add(id);
    return { $value: id };
  };
  function eventValue(source: unknown): Wire {
    if (source === null || typeof source !== "object") {
      throw new TypeError("event source must be an object");
    }
    let sourceId = eventSources.get(source);
    if (sourceId === undefined) {
      sourceId = identity();
      eventSources.set(source, sourceId);
    }
    const id = identity();
    values.set(id, source);
    allocations.at(-1)?.values.add(id);
    return { $value: id, $source: sourceId };
  }
  function encode(value: unknown): Wire {
    if (value === undefined) {
      return { $undefined: true };
    }
    if (typeof value === "function") {
      const id = identity();
      // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- This private wire boundary decodes the locally built Rust ABI after its shape checks.
      callbacks.set(id, value as (...args: unknown[]) => unknown);
      allocations.at(-1)?.callbacks.add(id);
      return { $callback: id };
    }
    if (
      value === null ||
      typeof value === "string" ||
      typeof value === "boolean" ||
      typeof value === "number"
    ) {
      return value;
    }
    if (typeof value !== "object") {
      throw new TypeError("unsupported native bridge value");
    }
    const raw = handles.get(value);
    if (raw) {
      return { $handle: raw.id, $kind: raw.kind };
    }
    if (Array.isArray(value)) {
      return value.map(encode);
    }
    const record: Record<string, Wire> = {};
    for (const [key, item] of Object.entries(value)) {
      record[key] = key === "projection" ? retain(item) : encode(item);
    }
    return record;
  }
  function decode(value: unknown): unknown {
    if (value === null || typeof value !== "object") {
      return value;
    }
    if (Array.isArray(value)) {
      return value.map(decode);
    }
    const data = fields(value);
    if (typeof data.$portable === "string") {
      return JSON.parse(data.$portable);
    }
    if (data.$next !== undefined) {
      const next = fields(decode(data.$next));
      const { call, recover } = next;
      if (typeof call !== "function" || typeof recover !== "function") {
        throw new TypeError("invalid issued hook continuation");
      }
      return Object.freeze({
        call: async (input: unknown): Promise<unknown> =>
          await Reflect.apply(call, next, [portableRead(input)]),
        recover: async (input: unknown): Promise<unknown> =>
          await Reflect.apply(recover, next, [portableRead(input)]),
      });
    }
    if (data.$undefined === true) {
      return undefined;
    }
    if (typeof data.$value === "string") {
      const result = values.get(data.$value);
      if (data.$take === true) {
        values.delete(data.$value);
      }
      return result;
    }
    if (typeof data.$handle === "string" && typeof data.$kind === "string") {
      const { $handle: id, $kind: kind } = data;
      const known = issued.get(id)?.deref();
      if (known) {
        return known;
      }
      // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- This private wire boundary decodes the locally built Rust ABI after its shape checks.
      const target = Object.create(null) as object;
      const wrapper = new Proxy(target, {
        get(_target, method) {
          if (method === "then" || typeof method !== "string") {
            return undefined;
          }
          return (...args: unknown[]) => request(method, id, args);
        },
      });
      handles.set(wrapper, { id, kind });
      issued.set(id, new WeakRef(wrapper));
      finalized.register(wrapper, id, wrapper);
      return wrapper;
    }
    if (typeof data.$job === "string") {
      const id = data.$job;
      // oxlint-disable-next-line promise/avoid-new -- Rust settles this observer through a later native event.
      return new Promise((resolve, reject) => {
        jobs.set(id, { resolve, reject });
      });
    }
    return Object.fromEntries(Object.entries(data).map(([key, child]) => [key, decode(child)]));
  }
  function capture<T>(operation: () => { adopted: unknown; result: () => T }): T {
    const scope = { callbacks: new Set<string>(), values: new Set<string>() };
    allocations.push(scope);
    let adopted: RecordValue = {};
    try {
      const completed = operation();
      adopted = fields(completed.adopted);
      return completed.result();
    } finally {
      allocations.pop();
      for (const id of scope.callbacks) {
        if (!Array.isArray(adopted.callbacks) || !adopted.callbacks.includes(id)) {
          callbacks.delete(id);
        }
      }
      for (const id of scope.values) {
        if (!Array.isArray(adopted.values) || !adopted.values.includes(id)) {
          values.delete(id);
        }
      }
    }
  }
  function flush() {
    if (disposed || polling) {
      return;
    }
    polling = true;
    try {
      const events = capture(() => {
        const envelope = fields(JSON.parse(native.poll()));
        const { events: rawEvents } = envelope;
        if (!isMessages(rawEvents)) {
          throw new TypeError("invalid native poll envelope");
        }
        return { adopted: envelope.adopted, result: () => rawEvents };
      });
      for (const item of events) {
        const event = fields(item);
        if (typeof event.releaseCallback === "string") {
          callbacks.delete(event.releaseCallback);
        } else if (typeof event.releaseValue === "string") {
          values.delete(event.releaseValue);
        } else if (typeof event.job === "string") {
          const job = jobs.get(event.job);
          jobs.delete(event.job);
          if (job) {
            if ("error" in event) {
              job.reject(failure(event.error));
            } else {
              job.resolve(decode(event.ok));
            }
          }
        }
      }
    } finally {
      polling = false;
    }
  }
  function request(operation: string, handle: string, args: unknown[]): unknown {
    if (disposed) {
      throw new Error("Host is closed");
    }
    return capture(() => {
      const encoded = args.map((value, index) =>
        operation === "onEvent" && index === 1 ? eventValue(value) : encode(value),
      );
      const envelope = fields(
        JSON.parse(native.request(operation, handle, JSON.stringify(encoded))),
      );
      return {
        adopted: envelope.adopted,
        result: () => {
          if ("error" in envelope) {
            throw failure(envelope.error);
          }
          const result = decode(envelope.ok);
          if (operation === "free") {
            const wrapper = issued.get(handle)?.deref();
            if (wrapper !== undefined) {
              finalized.unregister(wrapper);
            }
            issued.delete(handle);
          }
          return result;
        },
      };
    });
  }

  async function settle(job: string, result: unknown, message: RecordValue): Promise<void> {
    let settlement: string;
    try {
      const value = await result;
      let encoded: Wire;
      if (message.portable === 1 || message.portable === 2) {
        let payload = value;
        if (message.portable === 2 && value !== null && typeof value === "object") {
          const own = Object.getOwnPropertyDescriptors(value);
          if (
            own.kind?.value === "stop" &&
            own.value &&
            "value" in own.value &&
            own.value.value === undefined &&
            Object.keys(own).length === 2 &&
            Reflect.ownKeys(value).length === 2 &&
            (Object.getPrototypeOf(value) === Object.prototype ||
              Object.getPrototypeOf(value) === null)
          ) {
            payload = { kind: "stop" };
          }
        }
        encoded = canonicalJson(
          payload === undefined ? { present: false } : { present: true, value: payload },
        );
      } else if (message.discard === true) {
        encoded = encode(undefined);
      } else if (message.retain === true) {
        encoded = retain(value);
      } else {
        encoded = encode(value);
      }
      settlement = JSON.stringify({ ok: encoded });
    } catch (error) {
      settlement = JSON.stringify({ error: rejection(error) });
    }
    native.complete(job, settlement);
  }
  function dispatch(text: string): string {
    const message = fields(JSON.parse(text));
    try {
      if (typeof message.eventSource === "string") {
        const source = fields(values.get(message.eventSource));
        const { subscribe } = source;
        if (typeof subscribe !== "function") {
          throw new TypeError("event source requires subscribe");
        }
        const detach: unknown = Reflect.apply(subscribe, source, [
          (value: unknown) => {
            const raw = fields(message.sink);
            const result = fields(
              JSON.parse(
                native.request("emit", String(raw.$handle), JSON.stringify([retain(value)])),
              ),
            );
            if ("error" in result) {
              throw failure(result.error);
            }
          },
        ]);
        if (typeof detach !== "function") {
          throw new TypeError("event subscription requires synchronous cleanup");
        }
        return JSON.stringify({ ok: encode(detach) });
      }
      const callback = callbacks.get(String(message.callback));
      if (!callback) {
        throw new Error("callback is no longer owned by Rust");
      }
      // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- This private wire boundary decodes the locally built Rust ABI after its shape checks.
      const args = decode(message.args) as unknown[];
      // oxlint-disable-next-line promise/prefer-await-to-callbacks,node/callback-return -- Synchronous Node-API dispatch must return before Promise settlement.
      const result = callback(...args);
      if (typeof message.job === "string") {
        const { job } = message;
        // The observer always settles its admitted Rust channel; completion failures surface as transport errors.
        // oxlint-disable-next-line promise/prefer-await-to-callbacks -- This observer must not block synchronous Node-API dispatch.
        void settle(job, result, message).catch((error: unknown) => {
          queueMicrotask(() => {
            throw error;
          });
        });
        return JSON.stringify({ ok: { $undefined: true } });
      }
      if (
        result !== null &&
        (typeof result === "object" || typeof result === "function") &&
        typeof Reflect.get(result, "then") === "function"
      ) {
        // Observe a refused asynchronous declaration to avoid an unrelated unhandled rejection.
        void Promise.resolve(result).catch(() => {
          // Refused declaration errors are already reported synchronously.
        });
        throw Object.assign(new Error("declaration callback must return synchronously"), {
          reason: "async_declaration",
          code: "invalid",
        });
      }
      if (message.discard === true) {
        return JSON.stringify({ ok: encode(undefined) });
      }
      return JSON.stringify({ ok: message.retain === true ? eventValue(result) : encode(result) });
    } catch (error) {
      return JSON.stringify({ error: rejection(error) });
    }
  }
  const timer = setInterval(flush, 1);
  return {
    authority,
    counts: () =>
      Object.freeze({
        callbacks: callbacks.size,
        values: values.size,
        wrappers: issued.size,
        jobs: jobs.size,
      }),
    // oxlint-disable-next-line typescript/no-unnecessary-type-parameters -- The matching Rust operation determines its checked return type.
    request<T = unknown>(operation: string, args: unknown[] = []): T {
      // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- Only the private facade selects types for its matching checked Rust operations.
      return request(operation, "", args) as T;
    },
    flush,
    dispose() {
      disposed = true;
      clearInterval(timer);
      callbacks.clear();
      values.clear();
      issued.clear();
      for (const job of jobs.values()) {
        job.reject(new Error("Host is closed"));
      }
      jobs.clear();
    },
  };
}

export type NativeTransport = ReturnType<typeof nativeTransport>;
