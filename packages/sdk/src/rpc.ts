import { Errors } from "./errors.ts";
import { RpcEvent, RpcOp } from "./protocol.ts";
import { Code, Status } from "./identifiers.ts";
import type { Host } from "./host.ts";
import type { Context } from "./runtime/context.ts";
import type { DynamicReference } from "./types.ts";
import type { Codec } from "./codec.ts";
import { jsonCodec } from "./codec.ts";
import { failure, TembraError } from "./error.ts";
import { protect } from "./values.ts";
import type { Readable } from "./state.ts";

/** Adapters for WebSocket, MessagePort, or an application channel implement this byte boundary. */
export interface Transport {
  send(frame: Uint8Array): void | Promise<void>;
  listen(receive: (frame: Uint8Array) => void, closed: (error?: unknown) => void): () => void;
  close(): void;
}
export interface Limits {
  codec?: Codec;
  maxFrame?: number;
  maxPending?: number;
  onError?: (error: unknown) => void;
}
interface Wire {
  id?: number;
  op?: RpcOp;
  payload?: any;
  ok?: any;
  error?: { code: Code; reason: string; message: string };
  event?: RpcEvent;
}
function errorValue(error: unknown) {
  const value = failure(error);
  return { code: value.code, reason: value.reason, message: value.message };
}

class Channel {
  readonly codec: Codec;
  readonly maxFrame: number;
  readonly maxPending: number;
  closed = false;
  #detach: () => void;
  constructor(
    readonly transport: Transport,
    readonly options: Limits,
    receive: (message: Wire) => void,
    readonly ended: (error: unknown) => void,
  ) {
    this.codec = options.codec ?? jsonCodec;
    this.maxFrame = options.maxFrame ?? 1024 * 1024;
    this.maxPending = options.maxPending ?? 128;
    this.#detach = transport.listen(
      (frame) => {
        if (this.closed) return;
        try {
          if (frame.length > this.maxFrame) throw new TembraError(Errors.Exhausted.FrameSize);
          const message = this.codec.deserialize(frame);
          if (!message || typeof message !== "object" || Array.isArray(message))
            throw new TembraError(Errors.Invalid.RpcFrame);
          receive(message);
        } catch (error) {
          options.onError?.(error);
          this.close(error);
        }
      },
      (error) => this.close(error),
    );
  }
  async send(message: Wire): Promise<void> {
    if (this.closed) throw new TembraError(Errors.Gone.Connection);
    const frame = this.codec.serialize(message);
    if (frame.length > this.maxFrame) throw new TembraError(Errors.Exhausted.FrameSize);
    await this.transport.send(frame);
  }
  close(error: unknown = new TembraError(Errors.Gone.Connection)): void {
    if (this.closed) return;
    this.closed = true;
    this.#detach?.();
    this.transport.close();
    this.ended(error);
  }
}

class RemoteState implements Readable<any> {
  #value: any;
  #listeners = new Set<(value: any) => void>();
  constructor(
    value: unknown,
    readonly check: () => void,
    readonly report: (error: unknown) => void,
  ) {
    this.#value = protect(value);
  }
  get value(): any {
    this.check();
    return this.#value;
  }
  adopt(value: unknown): void {
    this.#value = protect(value);
    for (const listener of this.#listeners) {
      try {
        listener(this.#value);
      } catch (error) {
        this.report(error);
      }
    }
  }
  subscribe(listener: (value: any) => void): () => void {
    this.check();
    this.#listeners.add(listener);
    listener(this.#value);
    return () => {
      this.#listeners.delete(listener);
    };
  }
  select<R>(compute: (value: any) => R): Readable<R> {
    const source = this;
    return {
      get value() {
        return compute(source.value);
      },
      select: (next) => source.select((value) => next(compute(value))),
      subscribe(listener) {
        let initial = true;
        let previous: R;
        return source.subscribe((value) => {
          const next = compute(value);
          if (initial || !Object.is(previous, next)) {
            initial = false;
            previous = next;
            listener(next);
          }
        });
      },
    };
  }
}

/** A lost connection revokes its references. Reconnect by creating a peer and acquiring current state. */
export class RemoteHost {
  readonly channel: Channel;
  #next = 0;
  #pending = new Map<number, { resolve(value: any): void; reject(error: unknown): void }>();
  #states = new Map<number, RemoteState>();
  constructor(transport: Transport, options: Limits = {}) {
    this.channel = new Channel(
      transport,
      options,
      (message) => {
        if (message.event === RpcEvent.State) {
          this.#states.get(message.payload.reference)?.adopt(message.payload.value);
          return;
        }
        const pending = message.id && this.#pending.get(message.id);
        if (!pending) return;
        this.#pending.delete(message.id!);
        if (message.error) pending.reject(new TembraError(message.error));
        else pending.resolve(message.ok);
      },
      (error) => {
        for (const pending of this.#pending.values()) pending.reject(error);
        this.#pending.clear();
        this.#states.clear();
      },
    );
  }
  async request(op: RpcOp, payload: unknown, signal?: AbortSignal): Promise<any> {
    signal?.throwIfAborted();
    if (this.#pending.size >= this.channel.maxPending)
      throw new TembraError(Errors.Exhausted.PendingRequests);
    const id = ++this.#next;
    if (!Number.isSafeInteger(id)) throw new TembraError(Errors.Exhausted.RequestIds);
    let abort: (() => void) | undefined;
    const result = new Promise((resolve, reject) => {
      this.#pending.set(id, { resolve, reject });
      abort = () => {
        this.#pending.delete(id);
        reject(signal?.reason ?? new TembraError(Errors.Cancelled.Request));
        void this.channel.send({ op: RpcOp.Cancel, payload: { id } }).catch(() => {});
      };
      signal?.addEventListener("abort", abort, { once: true });
      void this.channel.send({ id, op, payload }).catch((error) => {
        this.#pending.delete(id);
        reject(error);
      });
    });
    try {
      return await result;
    } finally {
      if (abort) signal?.removeEventListener("abort", abort);
    }
  }
  installedFacets(): Promise<unknown> {
    return this.request(RpcOp.Describe, null);
  }
  async get(
    service: string,
    key: string | readonly string[],
    options: { signal?: AbortSignal } = {},
  ): Promise<DynamicReference> {
    const acquired = await this.request(
      RpcOp.Get,
      { service, key: typeof key === "string" ? [key] : key },
      options.signal,
    );
    const id = acquired.reference as number;
    let released = false;
    const check = () => {
      if (released) throw new TembraError(Errors.Released.Reference);
      if (this.channel.closed) throw new TembraError(Errors.Gone.Connection);
    };
    const state = Object.hasOwn(acquired, "state")
      ? new RemoteState(acquired.state, check, (error) => this.channel.options.onError?.(error))
      : undefined;
    if (state) this.#states.set(id, state);
    const target = {
      ready: Promise.resolve(),
      get status() {
        return released ? Status.Released : peer.channel.closed ? Status.Gone : Status.Live;
      },
      get state() {
        check();
        if (!state) throw new TembraError(Errors.Invalid.State);
        return state;
      },
      release: () => {
        if (released) return;
        released = true;
        this.#states.delete(id);
        void this.request(RpcOp.Release, { reference: id }).catch((error) =>
          this.channel.options.onError?.(error),
        );
      },
      opaque() {
        throw new TembraError(
          Errors.Unsupported.RemoteOpaque,
          "Opaque resources are local to their language environment",
        );
      },
    };
    const peer = this;
    return new Proxy(target, {
      get: (target, property, receiver) => {
        if (property === "then") return undefined;
        if (Object.hasOwn(target, property)) return Reflect.get(target, property, receiver);
        if (typeof property !== "string" || !acquired.methods.includes(property)) return undefined;
        return async (...arguments_: unknown[]) => {
          check();
          const last = arguments_.at(-1) as Context | undefined;
          const signal =
            last && typeof last.check === "function" && "signal" in last
              ? (arguments_.pop() as Context).signal
              : undefined;
          return this.request(
            RpcOp.Call,
            { reference: id, method: property, arguments: arguments_ },
            signal,
          );
        };
      },
    }) as DynamicReference;
  }
  close(): void {
    this.channel.close();
  }
}

/** Trusted infrastructure supplies the principal and authorization policy; peers cannot choose it. */
export function serve(
  host: Host,
  transport: Transport,
  options: Limits & {
    principal: string;
    allow?: (service: string, key: readonly string[]) => boolean;
  },
): { close(): void } {
  type Claim = {
    reference: DynamicReference;
    methods: ReadonlySet<string>;
    detach?: () => void;
    pending?: unknown;
    sending: boolean;
    closed: boolean;
    active: boolean;
  };
  const references = new Map<number, Claim>();
  const calls = new Map<number, Context>();
  const requests = new Map<number, { cancelled: boolean }>();
  let next = 0;
  let pending = 0;
  const pump = (reference: number, record: Claim) => {
    if (record.sending || !record.active || record.closed) return;
    record.sending = true;
    void (async () => {
      while (Object.hasOwn(record, "pending") && !record.closed) {
        const value = record.pending;
        delete record.pending;
        await channel.send({ event: RpcEvent.State, payload: { reference, value } });
      }
    })()
      .catch((error) => channel.close(error))
      .finally(() => {
        record.sending = false;
      });
  };
  const channel = new Channel(
    transport,
    options,
    (message) => {
      if (message.op === RpcOp.Cancel) {
        const request = requests.get(message.payload?.id);
        if (request) request.cancelled = true;
        calls.get(message.payload?.id)?.cancel();
        return;
      }
      const id = message.id;
      if (!Number.isSafeInteger(id) || !id || id <= 0 || !message.op) {
        channel.close(new TembraError(Errors.Invalid.RpcRequest));
        return;
      }
      if (pending >= channel.maxPending || requests.has(id)) {
        void channel
          .send({ id, error: errorValue(new TembraError(Errors.Exhausted.PendingRequests)) })
          .catch(() => channel.close());
        return;
      }
      pending++;
      const request = { cancelled: false };
      requests.set(id, request);
      void (async () => {
        const payload = message.payload;
        switch (message.op) {
          case RpcOp.Describe:
            return host.installedFacets().map(({ describe, ...info }) => info);
          case RpcOp.Get: {
            if (
              typeof payload?.service !== "string" ||
              !Array.isArray(payload.key) ||
              !payload.key.every((key: unknown) => typeof key === "string")
            )
              throw new TembraError(Errors.Invalid.Address);
            if (options.allow && !options.allow(payload.service, payload.key))
              throw new TembraError(Errors.Forbidden.Service);
            if (references.size >= channel.maxPending)
              throw new TembraError(Errors.Exhausted.RemoteReferences);
            const reference = await host.get(payload.service, payload.key, {
              principal: options.principal,
            });
            if (channel.closed || request.cancelled) {
              reference.release();
              throw new TembraError(Errors.Gone.Connection);
            }
            const referenceId = ++next;
            const methods = new Set(host.session.reference(reference).methods);
            const record: Claim = {
              reference,
              methods,
              sending: false,
              closed: false,
              active: false,
            };
            references.set(referenceId, record);
            let snapshot: { state?: unknown } = {};
            try {
              snapshot = { state: reference.state.value };
              let initial = true;
              record.detach = reference.state.subscribe((value) => {
                if (initial) {
                  initial = false;
                  return;
                }
                record.pending = value;
                pump(referenceId, record);
              });
            } catch {
              /* State is optional; method-only services remain portable. */
            }
            return { reference: referenceId, methods: [...methods], ...snapshot };
          }
          case RpcOp.Release: {
            const record = references.get(payload?.reference);
            if (record) {
              record.closed = true;
              record.detach?.();
              record.reference.release();
              references.delete(payload.reference);
            }
            return null;
          }
          case RpcOp.Call: {
            const record = references.get(payload?.reference);
            if (
              !record ||
              typeof payload.method !== "string" ||
              !record.methods.has(payload.method) ||
              !Array.isArray(payload.arguments) ||
              typeof record.reference[payload.method] !== "function"
            )
              throw new TembraError(Errors.Forbidden.Method);
            if (request.cancelled) throw new TembraError(Errors.Cancelled.Request);
            return host.root(
              async (ctx) => {
                calls.set(id, ctx);
                try {
                  return (
                    (await record.reference[payload.method](...payload.arguments, ctx)) ?? null
                  );
                } finally {
                  calls.delete(id);
                }
              },
              { principal: options.principal },
            );
          }
          default:
            throw new TembraError(Errors.Invalid.RpcOperation);
        }
      })()
        .then(
          async (ok) => {
            await channel.send({ id, ok });
            if (message.op === RpcOp.Get) {
              const record = references.get(ok.reference);
              if (record) {
                record.active = true;
                pump(ok.reference, record);
              }
            }
          },
          (error) => channel.send({ id, error: errorValue(error) }),
        )
        .catch((error) => channel.close(error))
        .finally(() => {
          pending--;
          requests.delete(id);
        });
    },
    () => {
      for (const ctx of calls.values()) {
        try {
          ctx.cancel();
        } catch {}
      }
      for (const record of references.values()) {
        record.closed = true;
        record.detach?.();
        record.reference.release();
      }
      references.clear();
    },
  );
  return { close: () => channel.close() };
}
