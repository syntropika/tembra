import type { DynamicReference } from "../../authoring/types.ts";
import { Status } from "../../bridge/identifiers.ts";
import { RpcEvent, RpcOp } from "../../bridge/protocol.ts";
import { TembraError } from "../../error.ts";
import { Errors } from "../../errors.ts";
import type { Context } from "../../runtime/context.ts";

import { Channel, type Limits, type Transport } from "./channel.ts";
import { RemoteState } from "./state.ts";

/** A lost connection revokes its references. Reconnect by creating a peer and acquiring current state. */
export class RemoteHost {
  readonly channel: Channel;
  #next = 0;
  #pending = new Map<
    number,
    {
      /** Deliver the response to its pending caller. */
      resolve(value: any): void;
      /** Reject the caller when its request or connection fails. */
      reject(error: unknown): void;
    }
  >();
  #states = new Map<number, RemoteState>();

  /** Attach response routing and revoke state on disconnect. */
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

  /** Correlate one bounded remote request with optional cancellation. */
  request(op: RpcOp, payload: unknown, signal?: AbortSignal): Promise<any> {
    return this.#request(op, payload, signal);
  }

  /** Apply a response before resolving its caller so following frames see acquired state. */
  async #request(
    op: RpcOp,
    payload: unknown,
    signal?: AbortSignal,
    received?: (value: any) => void,
  ): Promise<any> {
    signal?.throwIfAborted();
    if (this.#pending.size >= this.channel.maxPending)
      throw new TembraError(Errors.Exhausted.PendingRequests);
    const id = ++this.#next;
    if (!Number.isSafeInteger(id)) throw new TembraError(Errors.Exhausted.RequestIds);
    let abort: (() => void) | undefined;
    const result = new Promise((resolve, reject) => {
      const pending = {
        resolve: (value: any) => {
          try {
            received?.(value);
            resolve(value);
          } catch (error) {
            if (op === RpcOp.Get) this.#release(value?.reference);
            reject(error);
          }
        },
        reject,
      };
      this.#pending.set(id, pending);
      abort = () => {
        if (this.#pending.get(id) !== pending) return;
        if (op === RpcOp.Get) {
          this.#pending.set(id, {
            resolve: (value) => this.#release(value?.reference),
            reject: () => {},
          });
        } else {
          this.#pending.delete(id);
        }
        reject(signal?.reason ?? new TembraError(Errors.Cancelled.Request));
        void this.channel.send({ op: RpcOp.Cancel, payload: { id } }).catch(() => {});
      };
      signal?.addEventListener("abort", abort, { once: true });
      void this.channel.send({ id, op, payload }).catch((error) => {
        if (this.#pending.get(id) !== pending) return;
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

  /** Release a late or unusable acquisition within the normal pending-request budget. */
  #release(reference: unknown): void {
    if (!Number.isSafeInteger(reference) || (reference as number) <= 0 || this.channel.closed)
      return;
    void this.request(RpcOp.Release, { reference }).catch((error) => {
      try {
        this.channel.options.onError?.(error);
      } catch {}
    });
  }

  /** Request remote orb metadata without service activation. */
  installedOrbs(): Promise<unknown> {
    return this.request(RpcOp.Describe, null);
  }

  /** Acquire a remote reference and its current state snapshot. */
  async get(
    service: string,
    key: string | readonly string[],
    options: { signal?: AbortSignal } = {},
  ): Promise<DynamicReference> {
    let released = false;
    const check = () => {
      if (released) throw new TembraError(Errors.Released.Reference);
      if (this.channel.closed) throw new TembraError(Errors.Gone.Connection);
    };
    let state: RemoteState | undefined;
    const acquired = await this.#request(
      RpcOp.Get,
      { service, key: typeof key === "string" ? [key] : key },
      options.signal,
      (value) => {
        if (
          !value ||
          !Number.isSafeInteger(value.reference) ||
          value.reference <= 0 ||
          !Array.isArray(value.methods) ||
          !value.methods.every((method: unknown) => typeof method === "string")
        )
          throw new TembraError(Errors.Invalid.RpcFrame);
        if (Object.hasOwn(value, "state")) {
          state = new RemoteState(value.state, check, (error) =>
            this.channel.options.onError?.(error),
          );
          this.#states.set(value.reference, state);
        }
      },
    );
    const id = acquired.reference as number;
    const target = {
      ready: Promise.resolve(),
      /** Read connection-aware status without sending a request. */
      get status() {
        return released ? Status.Released : peer.channel.closed ? Status.Gone : Status.Live;
      },
      /** Read the local mirror while the remote reference remains valid. */
      get state() {
        check();
        if (!state) throw new TembraError(Errors.Invalid.State);
        return state;
      },
      /** Release the remote claim once and stop retaining its state. */
      release: () => {
        if (released) return;
        released = true;
        this.#states.delete(id);
        void this.request(RpcOp.Release, { reference: id }).catch((error) =>
          this.channel.options.onError?.(error),
        );
      },
      /** Reject opaque resources that cannot cross the transport. */
      opaque() {
        throw new TembraError(
          Errors.Unsupported.RemoteOpaque,
          "Opaque resources are local to their language environment",
        );
      },
    };
    const peer = this;
    return new Proxy(target, {
      /** Expose only methods included in the acquired remote contract. */
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

  /** Close the connection and reject pending requests. */
  close(): void {
    this.channel.close();
  }
}
