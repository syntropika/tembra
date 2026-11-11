/**
 * One incoming HTTP exchange over a native typed carrier (Node NAPI, mobile UniFFI): the
 * protocol of `PHASE5_NATIVE_CARRIER_CONTRACT.md`, shared by `NodePort` and `MobileDriver`.
 * Platform adapters only translate their binding's names and byte types into `CarrierEvent`s
 * and `CarrierTransport` calls; JSON appears only in host requests and the completion envelope.
 *
 * Lifetime:
 * - The response head is published as soon as its `head` event arrives.
 * - The call (and so the caller's root and revision gate) ends only after `complete`, after
 *   every admitted host operation settled, and after `finish()`.
 * - Once `complete` was seen, or the exchange was abandoned, no pull, supply, disconnect or
 *   cancel is sent again: the same native port serves the revision's next exchange, so these
 *   would reach the wrong request. Only replies to host operations admitted before `complete`
 *   (drained before `finish`) and `finish` itself follow; both bindings cleared their reply
 *   maps at `complete` and never reuse ids, so such a late reply is refused and owns nothing.
 *
 * Request body: each `request-pull` awaits the application iterator's `next()` and answers
 * once. Its `return()` is called once, without being awaited, when the exchange ends (complete,
 * disconnect, cancellation, abandonment) before the body was read to its end. A `next()` still
 * pending at that point is abandoned, never awaited, so an application iterator that never
 * settles cannot hold the call.
 *
 * Refusal: a handler `error-code` arrives as `refused`, one WIT value in the standard binary
 * codec, before `complete`; the exchange reports it as `HttpHandlerError` after settlement. The
 * call itself still settles with its own completion (the untyped repeat, or a later trap,
 * deadline or cancellation). Malformed bytes fail the call; completion text is never parsed.
 *
 * Response body: each `next()` sends exactly one pull and awaits its frame. The body ends at
 * `end` (trailers resolve), at `error`, at the consumer's `return()` (a disconnect) or when the
 * call completes without a terminal frame (cancellation, trap, timeout): the latter is a failure,
 * never a truncated end.
 */
import { reply } from "./codec.ts";
import { failure as fault } from "../error.ts";
import { decodeAsyncHostRequest, decodeHostRequest } from "./host-protocol.ts";
import { decode, type HandleCodec, Reader } from "./standard-codec.ts";
import { METHODS, TYPES } from "./standard.ts";
import type { ErrorCode } from "wasi:http/types@0.3.0";
import {
  CONNECTION_TERMINATED,
  HttpHandlerError,
  type HttpHandlerResponse,
  type IncomingHttpRequest,
  type IngressExchange,
} from "./http-ingress.ts";
import type { HttpField } from "./http-provider.ts";
import type { ExecutionScope, IngressDispatch } from "./types.ts";

/** Carrier events in the SDK's byte form. */
export type CarrierEvent =
  | { kind: "host"; id: number; asynchronous: boolean; packet: string }
  /** The completion envelope JSON: `{"ok":…}` or `{"error":{code,reason,message}}`. */
  | { kind: "complete"; result: string }
  | { kind: "head"; status: number; fields: readonly HttpField[] }
  | { kind: "request-pull" }
  | { kind: "data"; data: Uint8Array }
  | { kind: "trailers"; fields: readonly HttpField[] }
  | { kind: "end" }
  | { kind: "error"; message: string }
  /** The handler returned its `error-code` (standard binary codec bytes); never with a head. */
  | { kind: "refused"; code: Uint8Array };

/** The answer to one `request-pull`: a chunk, the end with its trailers, or an error. */
export type RequestAnswer =
  | { kind: "data"; data: Uint8Array }
  | { kind: "end"; trailers: readonly HttpField[] }
  | { kind: "error"; message: string };

/** The native exchange's control calls; each may throw, none is called after `complete`. */
export interface CarrierTransport {
  /** Answer the pending request pull; `false` when the answer is no longer wanted. */
  supply(answer: RequestAnswer): boolean;
  /** Ask for one response frame. */
  pull(): void;
  /** The client went away. */
  disconnect(): void;
  /** The caller cancelled the call. */
  cancel(): void;
  reply(id: number, packet: string): boolean;
  /** Release the native ticket after `complete`. */
  finish(): void;
}

/** The control packet that starts the handler callback; the request travels typed. */
export function ingressPacket(dispatch: IngressDispatch): string {
  return JSON.stringify({
    kind: "ingress",
    environment: dispatch.environment,
    service: dispatch.service,
    method: dispatch.method,
  });
}

/** Encode one host reply without throwing through a native callback frame. */
export function respond(operation: () => unknown): string {
  try {
    return `{"ok":${reply(operation())}}`;
  } catch (error) {
    return JSON.stringify({ error: error instanceof Error ? error.message : String(error) });
  }
}

/** The `error-code` type: `wasi:http/client#send`'s error branch, which `handle` shares. */
const ERROR_CODE = (() => {
  const send = METHODS.find(
    (method) => method.interface === "wasi:http/client@0.3.0" && method.name === "send",
  );
  const result = send?.result === undefined ? undefined : TYPES[send.result];
  if (result?.[0] !== "result") throw new Error("generated tables lack wasi:http/client#send");
  return result[2] as number;
})();
const NO_HANDLES: HandleCodec = {
  handle() {
    throw new RangeError("an error code carries no handles");
  },
  value() {
    throw new RangeError("an error code carries no handles");
  },
};

/** Decode a native `refused` event: exactly one WIT `error-code` in the standard binary codec. */
export function refusedCode(bytes: Uint8Array): ErrorCode {
  if (!(bytes instanceof Uint8Array)) throw new TypeError("a refused code must be bytes");
  const reader = new Reader(bytes);
  const code = decode(reader, ERROR_CODE, NO_HANDLES) as ErrorCode;
  reader.end();
  return code;
}

/** A settable promise whose rejection is never reported as unhandled. */
function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((done, fail) => {
    resolve = done;
    reject = fail;
  });
  promise.catch(() => {});
  return { promise, resolve, reject };
}

/** Observe a value the application handed back, so a late rejection is never unhandled. */
function quiet(value: unknown): void {
  if (typeof (value as PromiseLike<unknown> | undefined)?.then === "function")
    Promise.resolve(value).catch(() => {});
}

/** Checked field pairs from the application; anything else is refused. */
function checkedFields(value: unknown): readonly HttpField[] {
  if (value === undefined || value === null) return [];
  if (!Array.isArray(value)) throw new TypeError("request trailers must be an array");
  return value.map((field) => {
    const [name, bytes] = Array.isArray(field) ? field : [];
    if (typeof name !== "string" || !(bytes instanceof Uint8Array))
      throw new TypeError("request trailers are [name, Uint8Array] pairs");
    return [name, bytes] as const;
  });
}

const ABANDONED = Symbol("abandoned");
type Frame = { kind: "data"; data: Uint8Array } | { kind: "trailers" } | { kind: "end" };

/** One exchange's protocol state; created per request, never reused. */
export class NativeIngress {
  /** Settles after `complete`, the drain of admitted host work and `finish()`. */
  readonly settled: Promise<void>;
  #settle!: { resolve: () => void; reject: (error: unknown) => void };
  /** `complete` was seen or the exchange was abandoned: the transport belongs to nobody. */
  #done = false;
  #failure: unknown;
  readonly #pending = new Set<Promise<void>>();
  #detach = () => {};

  // Request side.
  #iterator: AsyncIterator<Uint8Array> | undefined;
  #requestEnded = false;
  #requestClosed = false;
  readonly #requestStop = deferred<typeof ABANDONED>();
  #pulls = 0;
  #pulling = false;

  // Response side.
  #headed = false;
  #started = false;
  #ended = false;
  #bodyFailure: unknown;
  #trailerFields: readonly HttpField[] | undefined;
  readonly #trailers = deferred<readonly HttpField[] | undefined>();
  #frame: ReturnType<typeof deferred<Frame>> | undefined;
  #reading: Promise<unknown> = Promise.resolve();

  constructor(
    readonly transport: CarrierTransport,
    readonly exchange: IngressExchange,
    readonly scope: ExecutionScope,
  ) {
    this.settled = new Promise((resolve, reject) => {
      this.#settle = { resolve, reject };
    });
    // A rejected call is also reported through the exchange; the caller awaits `settled`.
    this.settled.catch(() => {});
    // This exchange owns the request from now on, body and trailers: every end below closes
    // the body, and a trailers rejection is answered to the guest when its body reaches the
    // end, never reported as unhandled meanwhile.
    quiet(exchange.own().trailers);
  }

  /** The request as the application supplied it. */
  get request(): IncomingHttpRequest {
    return this.exchange.request;
  }

  /** The native exchange has begun: connect the caller's cancellation to it. */
  begun(): void {
    const signal = this.scope.signal;
    if (!signal) return;
    const cancel = () => this.#cancel();
    this.#detach = () => signal.removeEventListener("abort", cancel);
    if (signal.aborted) cancel();
    else signal.addEventListener("abort", cancel, { once: true });
  }

  /** Handle one carrier event; never throws, so a native callback frame is never unwound. */
  event(event: CarrierEvent): void {
    if (this.#done) return;
    try {
      switch (event.kind) {
        case "host":
          return this.#host(event);
        case "request-pull":
          this.#pulls++;
          return void this.#serveRequest();
        case "head":
          return this.#head(event.status, event.fields);
        case "data":
          return this.#deliver({ kind: "data", data: event.data });
        case "trailers":
          this.#trailerFields = event.fields;
          return this.#deliver({ kind: "trailers" });
        case "end":
          this.#ended = true;
          this.#trailers.resolve(this.#trailerFields?.length ? this.#trailerFields : undefined);
          return this.#deliver({ kind: "end" });
        case "error": {
          const failure = new HttpHandlerError(
            { tag: "internal-error", val: event.message },
            event.message,
          );
          this.#endDelivery(failure);
          return;
        }
        case "refused":
          // Recorded now, reported by the exchange only after the call settled. Malformed
          // bytes are a transport failure; nothing is guessed from completion text.
          if (this.#headed) throw new Error("a refused exchange cannot have a head");
          return this.exchange.refuse(refusedCode(event.code));
        case "complete":
          // Backstop: an unexpected failure fails this call instead of escaping unobserved.
          return void this.#complete(event.result).catch((error) => this.#settle.reject(error));
        default:
          throw new Error("Unknown incoming HTTP carrier event");
      }
    } catch (error) {
      this.#broken(error);
    }
  }

  /**
   * The transport failed (its event stream broke): end both bodies, revoke and drain host work,
   * and reject. The platform adapter then quarantines its native exchange; `finish` is not called.
   */
  async abandon(error: unknown): Promise<void> {
    if (this.#done) return;
    this.#done = true;
    this.#detach();
    this.#endDelivery(error);
    this.#closeRequest();
    this.scope.cancel();
    await Promise.allSettled([...this.#pending]);
    this.#settle.reject(error);
  }

  /**
   * A failure inside this adapter or its platform mapping: keep it for the completion, and free
   * the guest from anything it waits on (disconnect, then cancel). The exchange still finishes.
   */
  fail(error: unknown): void {
    this.#broken(error);
  }

  #broken(error: unknown): void {
    this.#failure ??= error;
    // A frame may have been lost: the body can only fail now, never end as if complete.
    this.#endDelivery(error);
    this.#closeRequest();
    if (this.#done) return;
    for (const operation of [() => this.transport.disconnect(), () => this.transport.cancel()])
      try {
        operation();
      } catch {
        // The completion still arrives; the recorded failure is reported then.
      }
  }

  #cancel(): void {
    if (this.#done) return;
    try {
      this.transport.cancel();
    } catch (error) {
      this.#failure ??= error;
    }
  }

  #host(event: Extract<CarrierEvent, { kind: "host" }>): void {
    const { id, packet } = event;
    if (!event.asynchronous) {
      this.transport.reply(
        id,
        respond(() => this.scope.sync(decodeHostRequest(packet))),
      );
      return;
    }
    let request!: ReturnType<typeof decodeAsyncHostRequest>;
    const operation = Promise.resolve()
      .then(() => {
        request = decodeAsyncHostRequest(packet);
        return this.scope.async(request);
      })
      .then(
        (value) =>
          this.scope.deliver(request, value, (value) =>
            this.transport.reply(
              id,
              respond(() => value),
            ),
          ),
        (error) =>
          void this.transport.reply(
            id,
            respond(() => {
              throw error;
            }),
          ),
      )
      .catch((error) => {
        this.#failure ??= error;
      });
    this.#pending.add(operation);
    void operation.finally(() => this.#pending.delete(operation));
  }

  /** Answer request pulls one at a time, in order. */
  async #serveRequest(): Promise<void> {
    if (this.#pulling) return;
    this.#pulling = true;
    try {
      while (this.#pulls > 0 && !this.#done) {
        this.#pulls--;
        const answer = await this.#nextRequest();
        if (answer === ABANDONED || this.#done) return;
        try {
          this.transport.supply(answer);
        } catch (error) {
          this.#broken(error);
          return;
        }
      }
    } finally {
      this.#pulling = false;
    }
  }

  /** The next answer from the application's body, or `ABANDONED` once the exchange ended. */
  async #nextRequest(): Promise<RequestAnswer | typeof ABANDONED> {
    const stop = this.#requestStop.promise;
    try {
      if (this.#requestClosed) return ABANDONED;
      const { body, trailers } = this.request;
      if (body && !this.#requestEnded) {
        this.#iterator ??= body[Symbol.asyncIterator]();
        for (;;) {
          const pending = this.#iterator.next();
          quiet(pending);
          const next = await Promise.race([pending, stop]);
          if (next === ABANDONED) return ABANDONED;
          if (next.done) break;
          if (!(next.value instanceof Uint8Array))
            throw new TypeError("incoming request chunks must be Uint8Array");
          if (next.value.byteLength) return { kind: "data", data: next.value };
        }
      }
      this.#requestEnded = true;
      this.#iterator = undefined;
      if (!trailers) return { kind: "end", trailers: [] };
      const pairs = await Promise.race([Promise.resolve(trailers), stop]);
      if (pairs === ABANDONED) return ABANDONED;
      return { kind: "end", trailers: checkedFields(pairs) };
    } catch (error) {
      // A failed read ends the guest's body with `internal-error`; the iterator is done.
      this.#requestEnded = true;
      this.#iterator = undefined;
      return { kind: "error", message: error instanceof Error ? error.message : String(error) };
    }
  }

  /**
   * Stop the application's body once; its `return()` is never awaited. Every application call
   * here (the iterator factory of a never-read body, the `return` lookup and call) is guarded:
   * this runs on the completion, disconnect and abandonment paths, which must always finish.
   */
  #closeRequest(): void {
    if (this.#requestClosed) return;
    this.#requestClosed = true;
    this.#requestStop.resolve(ABANDONED);
    if (this.#requestEnded) return;
    const iterator = this.#iterator;
    this.#iterator = undefined;
    try {
      quiet((iterator ?? this.request.body?.[Symbol.asyncIterator]())?.return?.());
    } catch {
      // A body that cannot be opened or closed (a locked stream) cannot affect the exchange.
    }
  }

  /** Publish the response head; its body is pulled only as the consumer reads. */
  #head(status: number, fields: readonly HttpField[]): void {
    if (this.#headed) throw new Error("the response head was already published");
    this.#headed = true;
    const body = Object.freeze({
      [Symbol.asyncIterator]: () => this.#responseIterator(),
    });
    const response: HttpHandlerResponse = Object.freeze({
      status,
      fields: Object.freeze(fields.map((field) => Object.freeze(field) as HttpField)),
      body,
      trailers: this.#trailers.promise,
    });
    this.exchange.respond(response);
  }

  #responseIterator(): AsyncIterator<Uint8Array> {
    if (this.#started) throw new TypeError("the response body is read once");
    this.#started = true;
    return {
      next: () => {
        // Reads are sequential: one pull, one frame.
        const read = this.#reading.then(() => this.#next());
        this.#reading = read.catch(() => {});
        return read;
      },
      return: async () => {
        this.#disconnect();
        return { done: true, value: undefined };
      },
    };
  }

  async #next(): Promise<IteratorResult<Uint8Array>> {
    for (;;) {
      if (this.#ended) {
        if (this.#bodyFailure !== undefined) throw this.#bodyFailure;
        return { done: true, value: undefined };
      }
      const frame = deferred<Frame>();
      this.#frame = frame;
      try {
        this.transport.pull();
      } catch (error) {
        // The carrier already ended (a host-driven end whose completion is on its way): the
        // body fails; the call reports its own completion.
        this.#frame = undefined;
        this.#endDelivery(error);
        try {
          this.transport.disconnect();
        } catch {
          // Nothing is left to disconnect.
        }
        throw error;
      }
      const next = await frame.promise;
      if (next.kind === "data") return { done: false, value: next.data };
      if (next.kind === "end") return { done: true, value: undefined };
      // Trailers: the end follows on the next pull.
    }
  }

  /** Hand a frame to the read waiting for it; a frame nobody waits for is late and ignored. */
  #deliver(frame: Frame): void {
    const waiting = this.#frame;
    this.#frame = undefined;
    waiting?.resolve(frame);
  }

  /** End the delivery early: pending and later reads and the trailers fail with `failure`. */
  #endDelivery(failure: unknown): void {
    if (this.#ended) return;
    this.#ended = true;
    this.#bodyFailure = failure;
    this.#trailers.reject(failure);
    const waiting = this.#frame;
    this.#frame = undefined;
    waiting?.reject(failure);
  }

  /** The consumer stopped reading: the client disconnected. */
  #disconnect(): void {
    if (this.#ended) return;
    this.#endDelivery(new HttpHandlerError(CONNECTION_TERMINATED));
    this.#closeRequest();
    if (this.#done) return;
    try {
      this.transport.disconnect();
    } catch (error) {
      this.#broken(error);
    }
  }

  /** Physical completion: end what is still open, revoke and drain host work, then finish. */
  async #complete(result: string): Promise<void> {
    this.#done = true;
    this.#detach();
    let envelope: { ok?: unknown; error?: unknown };
    let error: unknown;
    try {
      envelope = JSON.parse(result);
      if (Object.hasOwn(envelope, "ok")) {
        if (typeof envelope.ok !== "string") throw new Error("Invalid guest result packet");
      } else error = envelope.error ?? "Interpreter operation failed";
    } catch (invalid) {
      error = invalid;
    }
    // A refusal's completion repeats it untyped; the typed `refused` event already reached the
    // exchange, whose head reports it. The call itself reports its own completion, so a trap,
    // deadline or cancellation stays the call's failure.
    const outcome = error !== undefined ? fault(error) : undefined;
    // A host-driven end (cancellation, trap, timeout) completes without a terminal frame.
    this.#endDelivery(outcome ?? new Error("the HTTP handler callback ended"));
    this.#closeRequest();
    this.scope.cancel();
    await Promise.allSettled([...this.#pending]);
    try {
      this.transport.finish();
    } catch (finishing) {
      this.#failure ??= finishing;
    }
    if (this.#failure !== undefined) return this.#settle.reject(this.#failure);
    if (outcome !== undefined) return this.#settle.reject(outcome);
    this.#settle.resolve();
  }
}
