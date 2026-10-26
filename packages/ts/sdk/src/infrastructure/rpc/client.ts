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
