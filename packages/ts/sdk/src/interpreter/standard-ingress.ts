/**
 * Incoming standard `wasi:http@0.3.0` in the browser `StandardHost`: the request handed to an
 * Orb's `wasi:http/handler#handle` and the delivery of the response it publishes.
 *
 * Ownership and ends:
 * - The request is an `own` resource issued to the callback. Its body pulls the application's
 *   `AsyncIterable` only as the handler reads; the host calls the iterator's `return()` when the
 *   body is dropped early, when the client disconnects, and when the call ends.
 * - The published response moves to the host. Its body is read only as the application reads.
 *   The delivery ends exactly once:
 *   - `eof`: the body and its trailers were read; the transmission result is `ok`, or the
 *     trailers' own error;
 *   - `disconnect`: the application stopped reading (`return()`); the handler sees
 *     `connection-terminated` on the transmission result, its request body is closed and its
 *     next write fails;
 *   - `abort`: the call was cancelled, timed out or the revision retired; same signals, and a
 *     pending or later read rejects with the reason.
 * The handler's callback stays open until that end (see `INGRESS_HEAD`).
 */
import type { ErrorCode } from "wasi:http/types@0.3.0";
import { type HttpField, HttpProviderError } from "./http-provider.ts";
import {
  CONNECTION_TERMINATED,
  type HttpHandlerResponse,
  HttpHandlerError,
  type IngressExchange,
} from "./http-ingress.ts";
import { Reader, STREAM_CHUNK } from "./standard-codec.ts";
import {
  type Body,
  err,
  HttpFields,
  HttpRequest,
  HttpResponse,
  internal,
  ok,
  type Outcome,
  type Trailers,
} from "./standard-http-values.ts";
import {
  abortable,
  type FutureReader,
  HostFuture,
  OutputStream,
  type StandardServices,
} from "./standard-streams.ts";

/** A standard code for a failure; non-standard failures are `internal-error`. */
function codeOf(error: unknown): ErrorCode {
  if (error instanceof HttpProviderError || error instanceof HttpHandlerError) return error.code;
  return internal(error instanceof Error ? error.message : String(error));
}

/** Received fields; an invalid name or value is the application's error. */
function fields(pairs: readonly HttpField[], what: string): HttpFields {
  try {
    return HttpFields.received(pairs);
  } catch {
    throw new TypeError(`invalid incoming ${what} field`);
  }
}

/** The request contents, pulled from the application's iterable as the handler reads. */
class IncomingBody implements Body {
  #iterator: AsyncIterator<Uint8Array> | undefined;
  #stopped = false;
  #terminated: ErrorCode | undefined;
  readonly #stop = new AbortController();

  constructor(
    readonly cleanup: (operation: () => unknown) => void,
    readonly source: AsyncIterable<Uint8Array> | null,
    readonly trailers: PromiseLike<readonly HttpField[] | undefined> | undefined,
  ) {}

  consume(_host: StandardServices, res: FutureReader<Outcome>): [OutputStream, HostFuture] {
    // The application has no use for the handler's processing outcome.
    res.close();
    let settle!: (trailers: Trailers) => void;
    const trailers = new Promise<Trailers>((resolve) => {
      settle = resolve;
    });
    const stream = new OutputStream(
      { next: () => this.#read(), close: (end) => end !== "eof" && this.#release() },
      (failure, end) => {
        if (end === "failed") return settle(err(codeOf(failure)));
        if (end === "dropped" || !this.trailers) return settle(ok(undefined));
        Promise.resolve(this.trailers).then(
          (pairs) => {
            try {
              settle(ok(pairs?.length ? fields(pairs, "trailer") : undefined));
            } catch (error) {
              settle(err(codeOf(error)));
            }
          },
          (error) => settle(err(codeOf(error))),
        );
      },
    );
    return [stream, new HostFuture(trailers)];
  }

  /** The next non-empty chunk, or `undefined` at the end; a terminated body fails the read. */
  async #read(): Promise<Uint8Array | undefined> {
    if (this.#terminated) throw new HttpProviderError(this.#terminated);
    if (!this.source || this.#stopped) return undefined;
    this.#iterator ??= this.source[Symbol.asyncIterator]();
    for (;;) {
      let next: IteratorResult<Uint8Array>;
      try {
        next = await abortable(this.#iterator.next(), this.#stop.signal);
      } catch (error) {
        if (this.#terminated) throw new HttpProviderError(this.#terminated);
        throw error;
      }
      if (next.done) {
        this.#iterator = undefined;
        return undefined;
      }
      if (!(next.value instanceof Uint8Array))
        throw new HttpProviderError(internal("incoming request chunks must be Uint8Array"));
      if (next.value.byteLength) return next.value;
    }
  }

  /**
   * Stop the application's iterable once; its `return()` is tracked as physical cleanup. The
   * application calls (the iterator factory of a never-read body, the `return` lookup and
   * call) run inside that cleanup, so a body that cannot be opened (a locked stream) or closed
   * fails only its own cleanup, never the guest call that dropped it.
   */
  #release(): void {
    if (this.#stopped) return;
    this.#stopped = true;
    const iterator = this.#iterator;
    const source = this.source;
    this.#iterator = undefined;
    if (iterator || source)
      this.cleanup(() => (iterator ?? source![Symbol.asyncIterator]()).return?.());
  }

  /** The exchange ended: a pending or later read fails with `code`. */
  terminate(code: ErrorCode): void {
    if (this.#terminated) return;
    this.#terminated = code;
    this.#stop.abort();
    this.#release();
  }

  /** Dropped unconsumed. */
  close(): void {
    this.#release();
  }
}

/** The response the handler published, read by the application. */
class Delivery {
  readonly ended: Promise<void>;
  readonly response: HttpHandlerResponse;
  #end!: () => void;
  #transmitted!: (outcome: Outcome) => void;
  /** Ends the application's trailers wait when the delivery ended early. */
  #stopTrailers!: (reason: unknown) => void;
  #stream: OutputStream;
  #finished = false;
  #started = false;
  #failure: unknown;

  constructor(
    readonly host: StandardServices,
    published: HttpResponse,
    readonly request: IncomingBody,
  ) {
    this.ended = new Promise((resolve) => {
      this.#end = resolve;
    });
    const transmitted = new Promise<Outcome>((resolve) => {
      this.#transmitted = resolve;
    });
    const status = published.getStatusCode();
    const pairs = published.headers.pairs();
    const body = published.take();
    // The response's own table reference moved here; its body now lives on alone.
    published.release();
    const [stream, future] = body.consume(host, {
      take: () => transmitted,
      close() {},
    });
    this.#stream = stream;
    const written = (future as HostFuture<Trailers>).value.then((result) => {
      if (result.tag === "err") throw new HttpHandlerError(result.val);
      const fields = result.val?.pairs();
      result.val?.release();
      return fields?.length ? fields : undefined;
    });
    const stopped = new Promise<never>((_, reject) => {
      this.#stopTrailers = reject;
    });
    const trailers = Promise.race([written, stopped]);
    for (const promise of [written, stopped, trailers]) promise.catch(() => {});
    const delivery = this;
    this.response = Object.freeze({
      status,
      fields: Object.freeze(pairs.map((pair) => Object.freeze(pair) as HttpField)),
      body: Object.freeze({
        [Symbol.asyncIterator]: () => delivery.#iterator(),
      }),
      trailers,
    });
  }

  #iterator(): AsyncIterator<Uint8Array> {
    if (this.#started) throw new TypeError("the response body is read once");
    this.#started = true;
    return {
      next: () => this.#next(),
      return: async () => {
        this.disconnect();
        return { done: true, value: undefined };
      },
    };
  }

  async #next(): Promise<IteratorResult<Uint8Array>> {
    if (this.#finished) {
      if (this.#failure !== undefined) throw this.#failure;
      return { done: true, value: undefined };
    }
    let bytes: Uint8Array;
    try {
      bytes = await this.#stream.read(STREAM_CHUNK, this.host.packetBytes);
    } catch (error) {
      if (this.#finished) throw this.#failure ?? error;
      this.#finish(err(codeOf(error)));
      throw error;
    }
    const reader = new Reader(bytes);
    if (reader.u8() === 0) {
      const chunk = reader.bytes();
      reader.end();
      return { done: false, value: chunk };
    }
    reader.end();
    // The body ended; the transmission completes with the trailers.
    try {
      await this.response.trailers;
    } catch (error) {
      this.#finish(err(codeOf(error)));
      throw error;
    }
    this.#finish(ok(undefined));
    return { done: true, value: undefined };
  }

  /** The application stopped reading: the client disconnected. */
  disconnect(): void {
    if (this.#finished) return;
    this.#stream.close();
    this.request.terminate(CONNECTION_TERMINATED);
    this.#stopTrailers(this.#failure ?? new HttpHandlerError(CONNECTION_TERMINATED));
    this.#finish(err(CONNECTION_TERMINATED));
  }

  /** The call ended before the delivery did; reads reject with `reason`. */
  abort(reason: unknown): void {
    if (this.#finished) return;
    this.#failure = reason;
    this.disconnect();
  }

  #finish(outcome: Outcome): void {
    if (this.#finished) return;
    this.#finished = true;
    this.#transmitted(outcome);
    this.#end();
  }
}

/** One incoming request served by one handler callback. */
export class IngressCall {
  readonly body: IncomingBody;
  #delivery: Delivery | undefined;

  constructor(
    readonly exchange: IngressExchange,
    cleanup: (operation: () => unknown) => void,
  ) {
    const request = exchange.own();
    this.body = new IncomingBody(cleanup, request.body, request.trailers);
  }

  /**
   * The `own<request>` resource handed to the handler; invalid input is refused here, and the
   * body this call already owns is closed with it.
   */
  resource(): HttpRequest {
    const parts = this.exchange.request;
    let headers: HttpFields;
    try {
      headers = fields(parts.fields, "request");
    } catch (error) {
      this.body.close();
      throw error;
    }
    const request = new HttpRequest(headers, undefined, this.body);
    const refuse = (what: string) => {
      request.release();
      return new TypeError(`invalid incoming request ${what}`);
    };
    if (request.setMethod({ tag: "other", val: parts.method }).tag === "err")
      throw refuse("method");
    if (parts.scheme !== undefined)
      if (request.setScheme({ tag: "other", val: parts.scheme }).tag === "err")
        throw refuse("scheme");
    if (request.setAuthority(parts.authority).tag === "err") throw refuse("authority");
    if (request.setPathWithQuery(parts.pathWithQuery).tag === "err")
      throw refuse("path with query");
    return request;
  }

  /**
   * Publish the handler's result. A response's head is released at once; the returned promise
   * resolves when its delivery ended. An error code is reported when the call settles.
   */
  respond(
    host: StandardServices,
    result: { tag: "ok"; val: HttpResponse } | { tag: "err"; val: ErrorCode },
  ): Promise<void> {
    if (result.tag === "err") {
      this.exchange.refuse(result.val);
      return Promise.resolve();
    }
    const delivery = new Delivery(host, result.val, this.body);
    this.#delivery = delivery;
    this.exchange.respond(delivery.response);
    return delivery.ended;
  }

  /** The callback ended or was cancelled: end the delivery and close the request body. */
  end(reason: unknown): void {
    this.#delivery?.abort(reason);
    this.body.terminate(CONNECTION_TERMINATED);
  }
}
