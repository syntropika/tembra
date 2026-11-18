import { reply } from "./codec.ts";
import type { Completion, Guest, Outcome } from "./core.ts";
import { GuestTrap } from "./core.ts";
import { decodeAsyncHostRequest, decodeHostRequest, type HostRequest } from "./host-protocol.ts";
import { combinedSignal } from "../runtime/signals.ts";
import type { StandardHost } from "./standard-host.ts";
import { DROP } from "./standard-codec.ts";
import type { ExecutionScope, IngressDispatch, InterpreterPort } from "./types.ts";
import type { IngressExchange } from "./http-ingress.ts";

/** The guest's question about the refusal of the synchronous call that just failed. */
const REFUSAL = '{"operation":"refusal"}';
/** The prefix of the guest's naming of the refusal its callback fails with. */
const REFUSED = '{"operation":"refused",';

/**
 * The host's refusals one callback's guest received, by the identity the guest names them by: the
 * effect that delivered one, or the number this port gave a synchronous one, which the guest asks
 * for at once. A callback that fails with one unchanged answers it, as the native executor does;
 * the guest can name only a refusal of the same callback.
 */
class Refusals {
  /** A guest rethrows a refusal it has just received, so only the most recent ones are kept. */
  static readonly KEPT = 64;
  readonly #requests = new Map<number, unknown>();
  readonly #effects = new Map<number, unknown>();
  #sequence = 0;
  /** The synchronous request that failed last, if the host refused it. */
  #last: number | undefined;
  /** The refusal the guest named as its failure. */
  named: { error: unknown } | undefined;

  /** Keep one refusal, forgetting the oldest beyond the bound. */
  static #keep(refusals: Map<number, unknown>, id: number, error: unknown): void {
    refusals.set(id, error);
    if (refusals.size > Refusals.KEPT) refusals.delete(refusals.keys().next().value!);
  }

  /** The host refused a synchronous request. */
  sync(error: unknown): void {
    const id = ++this.#sequence;
    Refusals.#keep(this.#requests, id, error);
    this.#last = id;
  }

  /** Another synchronous request was made. */
  forget(): void {
    this.#last = undefined;
  }

  /** The host refused an effect's request. */
  effect(id: number, error: unknown): void {
    Refusals.#keep(this.#effects, id, error);
  }

  /** Answer the guest's question about the synchronous request that just failed. */
  question(): string {
    const last = this.#last;
    this.#last = undefined;
    return last === undefined ? "null" : String(last);
  }

  /** Keep the refusal the guest names, if it is one of this callback's. */
  name(packet: string): void {
    let named: { effect?: unknown; request?: unknown } = {};
    try {
      named = JSON.parse(packet) as typeof named;
    } catch {
      // A malformed naming names nothing.
    }
    const [refusals, id] =
      typeof named.effect === "number" && named.request === undefined
        ? [this.#effects, named.effect]
        : typeof named.request === "number" && named.effect === undefined
          ? [this.#requests, named.request]
          : [undefined, undefined];
    this.named =
      refusals && id !== undefined && refusals.has(id) ? { error: refusals.get(id) } : undefined;
  }
}

/** Internal delivery journal; core ABI completion encoding ignores these host-owned identities. */
interface Delivery extends Completion {
  readonly issued: readonly number[] | undefined;
  readonly reads: readonly number[] | undefined;
}

/** A serialized Wasm heap yields data; host promises never enter its execution stack. */
export class BrowserPort implements InterpreterPort {
  guest: Guest | undefined;
  /** Revision-owned standard WASI host; absent until installation opens it. */
  standard: StandardHost | undefined;
  #description = "";
  #scope: ExecutionScope | undefined;
  #running = false;
  #closed = false;
  #poisoned = false;
  /** The host's refusals of the active callback. */
  #refusals = new Refusals();
  /** Retain per-installation I/O and logging independently of callback authority. */
  constructor(
    readonly timeout: number,
    readonly dispose: () => Promise<void>,
    readonly log: (record: Extract<HostRequest, { operation: "console" }>) => void,
    readonly packetBytes = 8 << 20,
  ) {}

  /** UTF-8 uses at most three bytes per UTF-16 code unit; only large packets need encoding. */
  private bounded(packet: string): string {
    if (
      packet.length > this.packetBytes ||
      (packet.length > Math.floor(this.packetBytes / 3) &&
        new TextEncoder().encode(packet).byteLength > this.packetBytes)
    )
      throw new Error("Interpreter packet exceeds its configured bound");
    return packet;
  }

  /** Bound a binary packet or reply by its exact length. */
  private boundedBytes(packet: Uint8Array): Uint8Array {
    if (packet.byteLength > this.packetBytes)
      throw new Error("Interpreter packet exceeds its configured bound");
    return packet;
  }

  /** Decode once and invoke the current host-owned scope without an event envelope. */
  sync(packet: string): string;
  sync(packet: Uint8Array): Uint8Array;
  sync(packet: string | Uint8Array): string | Uint8Array {
    if (packet instanceof Uint8Array) {
      this.#refusals.forget();
      // Standard calls exist only inside a callback; the revision's host owns their handles.
      if (!this.#scope || !this.standard) throw "No active host scope";
      try {
        if (packet[1] !== DROP) this.#scope.check();
        return this.boundedBytes(this.standard.sync(this.boundedBytes(packet)));
      } catch (error) {
        if (error instanceof GuestTrap) throw error;
        throw error instanceof Error ? error.message : String(error);
      }
    }
    // The guest's question about a refusal, and its naming of the one it fails with.
    if (packet === REFUSAL) return this.#refusals.question();
    if (packet.startsWith(REFUSED)) {
      this.#refusals.name(packet);
      return "null";
    }
    this.#refusals.forget();
    try {
      const request = decodeHostRequest(this.bounded(packet));
      if (request.operation === "console") {
        this.log(request);
        return "null";
      }
      if (!this.#scope) throw new Error("No active host scope");
      let value: unknown;
      try {
        value = this.#scope.sync(request);
      } catch (error) {
        this.#refusals.sync(error);
        throw error;
      }
      return this.bounded(reply(value));
    } catch (error) {
      throw error instanceof Error ? error.message : String(error);
    }
  }

  /** Observe cleanup and interruption in the same export result as the callback. */
  private value(outcome: Outcome): unknown {
    return JSON.parse(this.text(outcome));
  }

  /** Preserve the bounded result bytes for the prepared contract to check before decoding. */
  private text(outcome: Outcome): string {
    this.#poisoned ||= outcome.interrupted;
    this.bounded(outcome.value);
    // A callback that failed with the host's refusal unchanged answers it.
    if (!outcome.ok)
      throw this.#refusals.named ? this.#refusals.named.error : new Error(outcome.value);
    return outcome.value;
  }

  /** Any exception leaving the guest is a trap; the instance is never entered again. */
  private guard<T>(operation: () => T): T {
    try {
      return operation();
    } catch (error) {
      this.#poisoned = true;
      throw error;
    }
  }

  /** Refuse reuse until the previous stack and its I/O have physically returned. */
  private enter(scope?: ExecutionScope): void {
    if (this.#closed || this.#poisoned || this.#running)
      throw new Error("Interpreter is closed or busy");
    this.#running = true;
    this.#scope = scope;
    this.#refusals = new Refusals();
  }

  /** Initialize one revision before issuing any guest execution scope. */
  async load(packet: string): Promise<void> {
    this.enter();
    try {
      const input = this.bounded(packet);
      const outcome = this.guard(() => this.guest!.load(input));
      this.#description = this.bounded(outcome.value);
      this.value(outcome);
    } finally {
      this.#running = false;
    }
  }

  /** Read the validated source callback metadata once. */
  description(): string {
    return this.#description;
  }

  /** Factories receive only the synchronous declaration scope. */
  async declare(packet: string, scope: ExecutionScope): Promise<unknown> {
    this.enter(scope);
    try {
      const input = this.bounded(packet);
      return this.value(this.guard(() => this.guest!.declare(input)));
    } finally {
      this.#scope = undefined;
      this.#running = false;
    }
  }

  /**
   * Serve one incoming HTTP request: issue its request handle, then run the handler callback.
   * The callback (and so this call) lasts until the response delivery ended; whatever the
   * delivery had not finished when it ends is aborted with the callback's failure.
   */
  async ingress(
    dispatch: IngressDispatch,
    exchange: IngressExchange,
    scope: ExecutionScope,
  ): Promise<void> {
    if (this.#closed || this.#poisoned || this.#running)
      throw new Error("Interpreter is closed or busy");
    if (!this.standard) throw new Error("No standard WASI host");
    const request = this.standard.beginIngress(exchange);
    let failure: unknown = new Error("the HTTP handler callback ended");
    try {
      await this.execute(JSON.stringify({ kind: "ingress", ...dispatch, request }), scope);
    } catch (error) {
      failure = error;
      throw error;
    } finally {
      this.standard.endIngress(failure);
    }
  }

  /** Wait between steps in the host, preserving Promise.race and concurrent effect completion. */
  async execute(packet: string, scope: ExecutionScope): Promise<string> {
    this.enter(scope);
    const refusals = this.#refusals;
    const pending = new Map<number, Promise<Delivery>>();
    const ready: Delivery[] = [];
    let wake: (() => void) | undefined;
    let finished = false;
    let rejectDeadline!: (error: Error) => void;
    const deadline = new Promise<never>((_, reject) => {
      rejectDeadline = reject;
    });
    deadline.catch(() => {});
    // Standard calls observe the caller's cancellation and end with this callback.
    const ended = new AbortController();
    const signal = scope.signal ? combinedSignal(scope.signal, ended.signal) : ended.signal;
    // The one timeout of this callback, which poisons the port when it first happens.
    let timeout: Error | undefined;
    const expired = () => {
      if (!timeout) {
        timeout = new Error("Interpreter callback exceeded its wall time limit");
        this.#poisoned = true;
        ended.abort();
        scope.cancel();
      }
      return timeout;
    };
    const started = performance.now();
    const timer = setTimeout(() => rejectDeadline(expired()), this.timeout);
    // An outcome of a guest step after the deadline, a result or a failure, is a timeout too: a
    // step holds the thread past it and runs the timer late.
    const step = <V>(operation: () => V): V => {
      let outcome: { value: V } | { error: unknown };
      try {
        outcome = { value: this.guard(operation) };
      } catch (error) {
        outcome = { error };
      }
      if (performance.now() - started > this.timeout) throw expired();
      if ("error" in outcome) throw outcome.error;
      return outcome.value;
    };
    try {
      const input = this.bounded(packet);
      let progress = step(() => this.guest!.start(input));
      while (!progress.outcome) {
        if (timeout) throw timeout;
        for (const effect of progress.effects) {
          if (pending.size >= 128 || pending.has(effect.id))
            throw new Error("Invalid interpreter effect queue");
          const packet = effect.packet;
          let issued: readonly number[] | undefined;
          let reads: readonly number[] | undefined;
          const completed = (
            typeof packet === "string"
              ? Promise.resolve()
                  .then(() => decodeAsyncHostRequest(this.bounded(packet)))
                  .then((request) =>
                    scope.async(request).catch((error: unknown) => {
                      // The guest may fail with this refusal, by its effect's id.
                      refusals.effect(effect.id, error);
                      throw error;
                    }),
                  )
                  .then((value) => this.bounded(reply(value)))
              : Promise.resolve().then(async () => {
                  if (!this.standard) throw new Error("No standard WASI host");
                  scope.check();
                  const delivery = await this.standard.async(this.boundedBytes(packet), signal);
                  issued = delivery.issued;
                  reads = delivery.reads;
                  return this.boundedBytes(delivery.bytes);
                })
          )
            .then((value): Delivery => ({ id: effect.id, ok: true, value, issued, reads }))
            .catch((error): Delivery => ({
              id: effect.id,
              ok: false,
              issued,
              reads,
              // Standard failures carry the bare message, as the native dispatcher does.
              value:
                typeof packet !== "string" && error instanceof Error
                  ? error.message
                  : String(error),
            }))
            .then((response) => {
              ready.push(response);
              wake?.();
              return response;
            });
          pending.set(effect.id, completed);
        }
        if (!ready.length)
          await Promise.race([
            new Promise<void>((resolve) => {
              wake = resolve;
            }),
            deadline,
          ]);
        wake = undefined;
        const completed = ready.splice(0);
        for (const response of completed) {
          pending.delete(response.id);
          if (!response.ok)
            this.standard?.discard({ issued: response.issued ?? [], reads: response.reads });
        }
        progress = step(() => this.guest!.resume(completed));
      }
      finished = true;
      if (timeout) throw timeout;
      return this.text(progress.outcome);
    } finally {
      clearTimeout(timer);
      ended.abort();
      scope.cancel();
      if (!finished) {
        this.#poisoned = true;
        try {
          this.guest!.cancel();
        } catch {
          /* A trapped instance is never reused. */
        }
      }
      this.#scope = undefined;
      await Promise.allSettled(pending.values());
      for (const delivery of ready)
        this.standard?.discard({ issued: delivery.issued ?? [], reads: delivery.reads });
      // Input writers this callback left open end their streams, so no consumer outlives it.
      this.standard?.endCallback();
      await this.standard?.settled();
      this.#running = false;
    }
  }

  /** Pure guest cleanup needs no execution capability after the environment ends. */
  async release(environment: number): Promise<void> {
    if (this.#closed || this.#poisoned) return;
    await this.execute(JSON.stringify({ kind: "release", environment }), {
      check() {},
      cancel() {},
      deliver(_request, value, publish) {
        publish(value);
      },
      sync() {
        throw new Error("Release has no host scope");
      },
      async() {
        return Promise.reject(new Error("Release has no host scope"));
      },
    });
  }

  /** Dispose mounted providers only after their last physical operation has settled. */
  async close(): Promise<void> {
    if (this.#running) throw new Error("Guest still owns physical work");
    if (this.#closed) return;
    this.#closed = true;
    this.guest = undefined;
    const standard = this.standard;
    this.standard = undefined;
    try {
      await standard?.dispose();
    } finally {
      await this.dispose();
    }
  }
}
