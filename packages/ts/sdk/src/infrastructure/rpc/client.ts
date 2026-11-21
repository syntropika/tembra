import { checkSignal } from "../../runtime/signals.ts";
import type { DynamicReference } from "../../authoring/types.ts";
import { Status } from "../../bridge/identifiers.ts";
import { RpcEvent, RpcOp } from "../../bridge/protocol.ts";
import { TembraError } from "../../error.ts";
import { Errors } from "../../errors.ts";
import type { Context } from "../../runtime/context.ts";

import { Channel, type Limits, type Transport } from "./channel.ts";
import { RemoteState } from "./state.ts";
import { bindWitReference } from "../../runtime/wit-client.ts";
import { resourceBackend, resource, type RpcResource } from "./resources.ts";
import type { CapturedInvocation } from "../../runtime/adapters.ts";
import type { AcquisitionOptions } from "../../authoring/types.ts";

/** A lost connection revokes its references. Reconnect by creating a peer and acquiring current state. */
export class RemoteHost {
  readonly channel: Channel;
  #next = 0;
  #pending = new Map<
    number,
    {
      readonly control: boolean;
      /** Deliver the response to its pending caller. */
      resolve(value: any): void;
      /** Reject the caller when its request or connection fails. */
      reject(error: unknown): void;
    }
  >();
  #states = new Map<number, RemoteState>();
  #releases: {
    payload: unknown;
    bytes: number;
    signal: AbortSignal | undefined;
    resolve(value: unknown): void;
    reject(error: unknown): void;
  }[] = [];
  #releaseBytes = 0;

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
        this.#drainReleases();
      },
      (error) => {
        for (const pending of this.#pending.values()) pending.reject(error);
        this.#pending.clear();
        for (const queued of this.#releases) queued.reject(error);
        this.#releases = [];
        this.#releaseBytes = 0;
        this.#states.clear();
      },
    );
  }

  /** Correlate one bounded remote request with optional cancellation. */
  request(op: RpcOp, payload: unknown, signal?: AbortSignal): Promise<any> {
    return op === RpcOp.Release
      ? this.#queueRelease(payload, signal)
      : this.#request(op, payload, signal);
  }

  /** Buffer release bursts within one frame's byte budget while control replies are pending. */
  async #queueRelease(payload: unknown, signal?: AbortSignal): Promise<unknown> {
    if (signal) checkSignal(signal);
    if (this.channel.closed) throw new TembraError(Errors.Gone.Connection);
    const bytes = this.channel.codec.serialize({ op: RpcOp.Release, payload }).length;
    if (this.#releaseBytes + bytes > this.channel.maxFrame)
      throw new TembraError(Errors.Exhausted.FrameSize);
    return new Promise((resolve, reject) => {
      this.#releases.push({ payload, bytes, signal, resolve, reject });
      this.#releaseBytes += bytes;
      this.#drainReleases();
    });
  }

  /** Send queued releases as control slots become free without consuming ordinary capacity. */
  #drainReleases(): void {
    if (this.channel.closed) return;
    let pending = 0;
    for (const item of this.#pending.values()) if (item.control) pending++;
    while (pending < this.channel.maxPending && this.#releases.length) {
      const item = this.#releases.shift()!;
      this.#releaseBytes -= item.bytes;
      pending++;
      void this.#request(RpcOp.Release, item.payload, item.signal)
        .then(item.resolve, item.reject)
        .finally(() => this.#drainReleases());
    }
  }

  /** Apply a response before resolving its caller so following frames see acquired state. */
  async #request(
    op: RpcOp,
    payload: unknown,
    signal?: AbortSignal,
    received?: (value: any) => void,
  ): Promise<any> {
    if (signal) checkSignal(signal);
    // Releases use a separate, equally bounded window so ordinary work cannot block cleanup.
    const control = op === RpcOp.Release;
    let pendingCount = 0;
    for (const pending of this.#pending.values()) if (pending.control === control) pendingCount++;
    if (pendingCount >= this.channel.maxPending)
      throw new TembraError(Errors.Exhausted.PendingRequests);
    const id = ++this.#next;
    if (!Number.isSafeInteger(id)) throw new TembraError(Errors.Exhausted.RequestIds);
    let abort: (() => void) | undefined;
    let deliveryStarted = false;
    const result = new Promise((resolve, reject) => {
      const pending = {
        control,
        resolve: (value: any) => {
          try {
            received?.(value);
            if (
              op === RpcOp.Call &&
              typeof payload === "object" &&
              payload !== null &&
              "capabilities" in payload &&
              Array.isArray(value?.owned) &&
              value.owned.length > 0
            ) {
              void this.channel
                .send({ op: RpcOp.Accept, payload: { id } })
                .catch((error) => this.channel.close(error));
            }
            resolve(value);
          } catch (error) {
            if (op === RpcOp.Get) this.#release(value?.reference);
            else if (op === RpcOp.Call) this.channel.close(error);
            reject(error);
          }
        },
        reject,
      };
      this.#pending.set(id, pending);
      abort = () => {
        if (this.#pending.get(id) !== pending) return;
        if (
          deliveryStarted &&
          (op === RpcOp.Get ||
            (op === RpcOp.Call &&
              typeof payload === "object" &&
              payload !== null &&
              "capabilities" in payload))
        ) {
          this.#pending.set(id, {
            control,
            resolve: (value) => {
              if (op === RpcOp.Get) this.#release(value?.reference);
              else this.#releaseResult(value);
            },
            reject: () => {},
          });
        } else {
          this.#pending.delete(id);
        }
        reject(signal?.reason ?? new TembraError(Errors.Cancelled.Request));
        if (deliveryStarted)
          void this.channel.send({ op: RpcOp.Cancel, payload: { id } }).catch(() => {});
      };
      signal?.addEventListener("abort", abort, { once: true });
      void this.channel
        .send({ id, op, payload }, () => {
          // A codec may synchronously cancel while preparing the frame.
          if (signal) checkSignal(signal);
          deliveryStarted = true;
        })
        .catch((error) => {
          // A rejected send may already have delivered bytes, even if cancellation won first.
          if (
            deliveryStarted &&
            (op === RpcOp.Get ||
              (op === RpcOp.Call &&
                typeof payload === "object" &&
                payload !== null &&
                "capabilities" in payload))
          )
            this.channel.close(error);
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

  /** Release a late or unusable acquisition within the reserved control budget. */
  #release(reference: unknown): void {
    if (!Number.isSafeInteger(reference) || (reference as number) <= 0 || this.channel.closed)
      return;
    void this.request(RpcOp.Release, { reference }).catch((error) => this.channel.close(error));
  }

  /** Close a late resource result that its cancelled caller can no longer accept. */
  #releaseResult(value: unknown): void {
    try {
      if (
        typeof value === "object" &&
        value !== null &&
        "owned" in value &&
        Array.isArray(value.owned)
      )
        for (const item of value.owned) this.#releaseResource(resource(item));
    } catch (error) {
      this.channel.close(error);
    }
  }

  /** Release one checked resource through the same RPC claim operation as service references. */
  #releaseResource(item: RpcResource): void {
    if (this.channel.closed) return;
    void this.request(RpcOp.Release, { reference: item.reference, resource: item }).catch((error) =>
      this.channel.close(error),
    );
  }

  /** Request remote orb metadata without service activation. */
  installedOrbs(): Promise<unknown> {
    return this.request(RpcOp.Describe, null);
  }

  /** Acquire a remote reference and its current state snapshot. */
  async get(
    service: string,
    key: string | readonly string[],
    options: AcquisitionOptions & { signal?: AbortSignal } = {},
  ): Promise<DynamicReference> {
    let released = false;
    const check = () => {
      if (released) throw new TembraError(Errors.Released.Reference);
      if (this.channel.closed) throw new TembraError(Errors.Gone.Connection);
    };
    let state: RemoteState | undefined;
    const acquired = await this.#request(
      RpcOp.Get,
      { service, key: typeof key === "string" ? [key] : key, resources: 1 },
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
        this.#release(id);
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
    const reference = new Proxy(target, {
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
    bindWitReference(reference, (schema) =>
      resourceBackend(
        {
          identity: this,
          reference: id,
          resources: acquired.resources as unknown,
          signature: acquired.signature as unknown,
          check,
          request: async (payload, _capture: CapturedInvocation, received, signal) =>
            this.#request(RpcOp.Call, payload, signal, received),
          release: (item) => this.#releaseResource(item),
        },
        schema,
      ),
    );
    return reference;
  }

  /** Close the connection and reject pending requests. */
  close(): void {
    this.channel.close();
  }
}
