/**
 * Outgoing `wasi:http@0.3.0` (`types` and `client`) for trusted JCO bindings.
 *
 * Value semantics (fields, options, request targets) come from `standard-http-values.ts`, which follows
 * pinned upstream `wasmtime-wasi-http`. This module owns bodies and transport: the exact origin
 * grant is checked before any provider call, uploads stream the guest body with backpressure and
 * `Content-Length` enforcement, responses are published once their fields arrived, and body
 * reads honor the between-bytes timeout and the framed length. Provider work is physically owned
 * by the revision; guest-stream reads are bounded by its retirement. Incoming `wasi:http/handler`
 * requests and responses use the same resources through the private `ingress` bridge.
 */
import type { ErrorCode } from "wasi:http/types@0.3.0";
import {
  type HttpField,
  type HttpProvider,
  HttpProviderError,
  type HttpProviderRequest,
  type HttpProviderResponse,
} from "../http-provider.ts";
import {
  CONNECTION_TERMINATED,
  type HttpHandlerResponse,
  HttpHandlerError,
  type IncomingHttpRequest,
  type IngressExchange,
} from "../http-ingress.ts";
import {
  type HttpBody,
  digits,
  done,
  err,
  HttpFields,
  HttpRequest,
  HttpRequestOptions,
  HttpResponse,
  internal,
  ok,
  type Outcome,
  type Trailers,
} from "../standard-http-values.ts";
import {
  abortable,
  cancelled,
  closing,
  guestBytes,
  hostBytes,
  type WasiHost,
  type WasiInterfaces,
} from "./wasi-host.ts";

/** Compiled transport contents and trailers, consumed or dropped once. */
interface Body extends HttpBody {
  consume(res: unknown): [AsyncIterable<Uint8Array> | object, Promise<Trailers>];
}

/** Grant-checked selection of the host transport. */
export interface HttpClient {
  /** The provider granted for exactly this URL's origin, or `undefined` to deny. */
  provider(url: URL): HttpProvider | undefined;
}

/**
 * The per-instance incoming bridge for `wasi:http/handler` (private; wired by the revision). It
 * issues this instance's own `request` resource for an incoming request and delivers the
 * `response` the handler returned. Everything stays within the standard resources, streams and
 * futures; the application side is the existing `IngressExchange` carrier.
 */
export interface IngressBridge {
  /**
   * This instance's `request` for `incoming` (from `exchange.own()`); the guest owns it. Invalid
   * input throws a `TypeError` after closing the request body it already owns.
   */
  request(incoming: IncomingHttpRequest): unknown;
  /**
   * Publish the handler's `response` (this instance's class; anything else is refused) through
   * `exchange.respond` and deliver its body and trailers as the application reads them. Settles
   * when the delivery ended: resolves at the end of the body and trailers or when the application
   * stopped reading (its client disconnected); rejects on a body or trailers failure and when the
   * revision retired first.
   */
  respond(response: unknown, exchange: IngressExchange): Promise<void>;
}

/** Cases of `error-code`; a provider-supplied code outside them becomes `internal-error`. */
const ERROR_TAGS = new Set([
  "DNS-timeout",
  "DNS-error",
  "destination-not-found",
  "destination-unavailable",
  "destination-IP-prohibited",
  "destination-IP-unroutable",
  "connection-refused",
  "connection-terminated",
  "connection-timeout",
  "connection-read-timeout",
  "connection-write-timeout",
  "connection-limit-reached",
  "TLS-protocol-error",
  "TLS-certificate-error",
  "TLS-alert-received",
  "HTTP-request-denied",
  "HTTP-request-length-required",
  "HTTP-request-body-size",
  "HTTP-request-method-invalid",
  "HTTP-request-URI-invalid",
  "HTTP-request-URI-too-long",
  "HTTP-request-header-section-size",
  "HTTP-request-header-size",
  "HTTP-request-trailer-section-size",
  "HTTP-request-trailer-size",
  "HTTP-response-incomplete",
  "HTTP-response-header-section-size",
  "HTTP-response-header-size",
  "HTTP-response-body-size",
  "HTTP-response-trailer-section-size",
  "HTTP-response-trailer-size",
  "HTTP-response-transfer-coding",
  "HTTP-response-content-coding",
  "HTTP-response-timeout",
  "HTTP-upgrade-failed",
  "HTTP-protocol-error",
  "loop-detected",
  "configuration-error",
  "internal-error",
]);

/** Map a failure to an `error-code`; `fallback` names the phase-specific generic code. */
function errorCode(error: unknown, fallback: (message: string) => ErrorCode = internal): ErrorCode {
  if (error instanceof HttpProviderError || error instanceof HttpHandlerError) {
    const code = error.code as { tag?: unknown } | undefined;
    if (typeof code?.tag === "string" && ERROR_TAGS.has(code.tag)) return code as ErrorCode;
  }
  return fallback(error instanceof Error ? error.message : String(error));
}

/** Nanoseconds to timer milliseconds; a duration beyond the timer range means no timer. */
function millis(duration: bigint | undefined): number | undefined {
  if (duration === undefined) return undefined;
  const ms = (duration + 999_999n) / 1_000_000n;
  return ms > 0x7fffffffn ? undefined : Number(ms);
}

/** Read a guest `future<T>` (a thenable or a JCO reader), bounded by `retired`. */
async function readFuture<T>(future: unknown, retired: AbortSignal): Promise<T> {
  const value = future as { then?: unknown; read?: () => Promise<unknown> };
  if (typeof value?.then === "function")
    return (await abortable(value as Promise<T>, retired)) as T;
  if (typeof value?.read === "function") {
    const read = await abortable(value.read(), retired);
    return (
      read && typeof read === "object" && "done" in read && "value" in read
        ? (read as { value: unknown }).value
        : read
    ) as T;
  }
  throw new TypeError("expected a guest future");
}

/** Wait for `pending` until `signal` aborts or `ms` passes. */
function deadline<T>(
  pending: Promise<T>,
  signal: AbortSignal | undefined,
  ms: number | undefined,
): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    let timer: ReturnType<typeof setTimeout> | undefined;
    const stop = () => {
      clearTimeout(timer);
      signal?.removeEventListener("abort", abort);
    };
    const abort = () => {
      stop();
      reject(cancelled());
    };
    // Observe the physical promise even if cancellation already happened before this wait.
    pending.then(
      (value) => (stop(), resolve(value)),
      (error) => (stop(), reject(error)),
    );
    if (signal?.aborted) return abort();
    signal?.addEventListener("abort", abort, { once: true });
    if (ms !== undefined)
      timer = setTimeout(() => {
        stop();
        reject(new HttpProviderError({ tag: "connection-read-timeout" } as never));
      }, ms);
  });
}

/** A response's framed body length; `undefined` where HTTP defines no body or none was declared. */
function framedLength(method: string, status: number, fields: HttpFields): bigint | undefined {
  if (method === "HEAD" || status < 200 || status === 204 || status === 304) return undefined;
  const declared = new Set(fields.get("content-length").map(digits));
  if (declared.size > 1)
    throw new HttpProviderError(
      internal("the HTTP provider returned conflicting lengths") as never,
    );
  const [length] = declared;
  return length === undefined ? undefined : BigInt(length);
}

/** The `wasi:http/types` and `client` import objects of one instance. */
export function httpImports(
  host: WasiHost,
  client: HttpClient | undefined,
): WasiInterfaces<"types" | "client"> & { ingress: IngressBridge } {
  /** A body the guest wrote: its contents stream, its trailers future and its transmission result. */
  class GuestBody implements Body {
    readonly result: Promise<Outcome>;
    #settle!: (outcome: Outcome) => void;
    #taken = false;
    /** The trailers future reader was taken (read for a consumer, or released). */
    #trailersTaken = false;
    constructor(
      readonly contents: unknown,
      readonly trailers: unknown,
    ) {
      this.result = new Promise((resolve) => (this.#settle = resolve));
    }
    settle(outcome: Outcome): void {
      this.#settle(outcome);
    }
    /** Take the trailers future reader exactly once. */
    #readTrailers(): Promise<Trailers> {
      this.#trailersTaken = true;
      return readFuture<Trailers>(this.trailers, host.retired);
    }
    /** Pass the guest's own stream and trailers through to the consumer. */
    consume(res: unknown): [object, Promise<Trailers>] {
      this.#taken = true;
      readFuture<Outcome>(res, host.retired).then(
        (outcome) => this.settle(outcome),
        () => this.settle(done),
      );
      const contents =
        this.contents ??
        hostBytes(
          async () => undefined,
          () => {},
          host.retired,
        );
      return [contents as object, this.#readTrailers()];
    }
    /**
     * Release a body nobody will read (a refused `send`, a message dropped unconsumed), once:
     * the guest's contents stream is dropped, so its writer sees the reader gone, and the
     * trailers future is read and its value released. JCO gives the host a guest future only as
     * a lazy thenable without a drop, and the guest's future write completes only when its reader
     * reads; left alone, that write keeps the guest's task suspended.
     */
    discard(): void {
      if (!this.#taken) {
        this.#taken = true;
        try {
          (this.contents as { [Symbol.dispose]?: () => void } | undefined)?.[Symbol.dispose]?.();
        } catch {
          /* Already gone with its instance. */
        }
      }
      if (this.#trailersTaken) return;
      const trailers = this.trailers as { then?: unknown; read?: unknown } | null;
      if (typeof trailers?.then !== "function" && typeof trailers?.read !== "function") {
        this.#trailersTaken = true;
        return;
      }
      this.#readTrailers().then(
        (value) => {
          if (value?.tag === "ok") value.val?.release();
        },
        () => {},
      );
    }
    /**
     * The upload a provider reads, with `Content-Length` enforced. `completed` resolves `true` at
     * the guest's end of body, `false` when the provider stopped reading early (as upstream, not a
     * transmission failure by itself), and rejects only on a body failure: a length mismatch or a
     * lost guest stream. The trailers' own error reaches the provider through `trailers`.
     */
    upload(declared: bigint | undefined): {
      body: ReadableStream<Uint8Array> | null;
      trailers: Promise<readonly [string, Uint8Array][] | undefined>;
      completed: Promise<boolean>;
      /**
       * The exchange failed: release the guest's body writer even when the provider never read
       * or cancelled the upload, and fail the provider's pending or later reads.
       */
      abort(reason: unknown): void;
    } {
      this.#taken = true;
      let complete!: (finished: boolean) => void;
      let fail!: (error: unknown) => void;
      const completed = new Promise<boolean>(
        (resolve, reject) => ((complete = resolve), (fail = reject)),
      );
      completed.catch(() => {});
      const trailers = this.#readTrailers().then((value) => {
        if (value.tag === "err") throw new HttpProviderError(value.val as never);
        const pairs = value.val?.pairs();
        value.val?.release();
        return pairs;
      });
      trailers.catch(() => {});
      if (this.contents === undefined) {
        if (declared !== undefined && declared !== 0n) {
          fail(
            new HttpProviderError({
              tag: "HTTP-request-body-size",
              val: 0n,
            } as never),
          );
        } else trailers.then(() => complete(true), fail);
        return { body: null, trailers, completed, abort() {} };
      }
      const input = guestBytes(this.contents, host.retired);
      let sent = 0n;
      let stopped = false;
      let source: ReadableStreamDefaultController<Uint8Array> | undefined;
      const body = new ReadableStream<Uint8Array>(
        {
          start(controller) {
            source = controller;
          },
          async pull(controller) {
            try {
              const next = await input.next();
              if (stopped) return;
              if (next.done) {
                if (declared !== undefined && sent !== declared)
                  throw new HttpProviderError({
                    tag: "HTTP-request-body-size",
                    val: sent,
                  } as never);
                // EOF is successful only after the guest's completion future succeeded.
                // Never let a provider commit bytes whose trailers later report failure.
                await trailers;
                if (stopped) return;
                stopped = true;
                controller.close();
                complete(true);
                return;
              }
              sent += BigInt(next.value.byteLength);
              if (declared !== undefined && sent > declared)
                throw new HttpProviderError({
                  tag: "HTTP-request-body-size",
                  val: sent,
                } as never);
              controller.enqueue(next.value);
            } catch (error) {
              if (stopped) return;
              stopped = true;
              void input.return!();
              fail(error);
              controller.error(error);
            }
          },
          cancel() {
            // The provider stopped reading: the guest's writer sees its reader dropped.
            if (stopped) return;
            stopped = true;
            void input.return!();
            complete(false);
          },
        },
        // Strict backpressure: nothing is read from the guest before the provider asks for it.
        { highWaterMark: 0 },
      );
      const abort = (reason: unknown) => {
        if (stopped) return;
        stopped = true;
        void input.return!();
        try {
          source?.error(reason);
        } catch {
          /* Already closed or errored. */
        }
        fail(reason);
      };
      return { body, trailers, completed, abort };
    }
    /** Dropped unconsumed: released, and its transmission result settles. */
    close(): void {
      if (this.#taken) return;
      this.discard();
      this.settle(done);
    }
  }

  /** A body received from a provider. */
  class ReceivedBody implements Body {
    #consumed = false;
    #released = false;
    readonly #retire = () => this.#release();
    constructor(
      readonly reader: ReadableStreamDefaultReader<Uint8Array> | undefined,
      readonly trailers: Promise<readonly HttpField[] | undefined> | undefined,
      readonly length: bigint | undefined,
      readonly between: number | undefined,
      readonly exchange: AbortController,
      readonly abortUpload: (reason: unknown) => void,
    ) {
      trailers?.catch(() => {});
      host.retired.addEventListener("abort", this.#retire, { once: true });
    }
    consume(res: unknown): [object, Promise<Trailers>] {
      this.#consumed = true;
      readFuture<Outcome>(res, host.retired).then(
        (outcome) => outcome.tag === "err" && this.#release(),
        () => this.#release(),
      );
      let received = 0n;
      let settle!: (value: Trailers) => void;
      const trailers = new Promise<Trailers>((resolve) => (settle = resolve));
      const reader = this.reader;
      const stream = hostBytes(
        async () => {
          for (;;) {
            const next = reader
              ? await host.physical(() =>
                  deadline(reader.read(), this.exchange.signal, this.between),
                )
              : { done: true as const, value: undefined };
            if (next.done) {
              if (this.length !== undefined && received !== this.length)
                throw new HttpProviderError({ tag: "HTTP-response-incomplete" } as never);
              return undefined;
            }
            if (!(next.value instanceof Uint8Array))
              throw new HttpProviderError(
                internal("the HTTP provider produced a non-byte chunk") as never,
              );
            if (!next.value.byteLength) continue;
            received += BigInt(next.value.byteLength);
            if (this.length !== undefined && received > this.length)
              throw new HttpProviderError({
                tag: "HTTP-response-body-size",
                val: received,
              } as never);
            return next.value;
          }
        },
        (end, failure) => {
          if (end !== "eof") {
            this.#release();
            settle(
              err(
                end === "failed"
                  ? errorCode(
                      failure,
                      (message) => ({ tag: "HTTP-protocol-error", val: message }) as never,
                    )
                  : internal("response body was dropped"),
              ),
            );
            return;
          }
          abortable(Promise.resolve(this.trailers), host.retired)
            .then((pairs) => settle(ok(pairs ? HttpFields.received(pairs) : undefined)))
            .catch((error) => {
              this.#release();
              settle(err(errorCode(error)));
            })
            .finally(() => host.retired.removeEventListener("abort", this.#retire));
        },
        host.retired,
      );
      return [stream, trailers];
    }
    close(): void {
      if (this.#consumed) return;
      this.#consumed = true;
      this.#release();
    }
    /** One physical exchange close, including a retained response or moved body at retirement. */
    #release(): void {
      if (this.#released) return;
      this.#released = true;
      host.retired.removeEventListener("abort", this.#retire);
      this.exchange.abort();
      this.abortUpload(
        new HttpProviderError(
          internal("response was released before its request body completed") as never,
        ),
      );
      const reader = this.reader;
      if (reader) closing(host, () => reader.cancel().catch(() => {}));
    }
  }

  class Request extends HttpRequest<Body> {
    static new(
      headers: HttpFields,
      contents: unknown,
      trailers: unknown,
      options: HttpRequestOptions | undefined,
    ) {
      const body = new GuestBody(contents, trailers);
      return [new Request(headers.frozen(), options?.timeouts, body), body.result];
    }
    static consumeBody(self: Request, res: unknown) {
      if (!(self instanceof Request)) throw new TypeError("Foreign HTTP request");
      return self.take().consume(res);
    }
    [Symbol.dispose](): void {
      this.release();
    }
  }
  class Response extends HttpResponse<Body> {
    static new(headers: HttpFields, contents: unknown, trailers: unknown) {
      const body = new GuestBody(contents, trailers);
      return [new Response(headers.frozen(), body), body.result];
    }
    static consumeBody(self: Response, res: unknown) {
      if (!(self instanceof Response)) throw new TypeError("Foreign HTTP response");
      return self.take().consume(res);
    }
    [Symbol.dispose](): void {
      this.release();
    }
  }

  /** A late or refused provider response has no guest owner for either body or trailers. */
  function discard(response: HttpProviderResponse | undefined): void {
    response?.trailers?.catch(() => {});
    const body = response?.body;
    if (body instanceof ReadableStream) closing(host, () => body.cancel().catch(() => {}));
  }

  /**
   * `client.send`. Ownership of the request moves at this first turn, so everything the exchange
   * needs is captured before the first `await`; the exact origin grant is checked before any
   * provider call.
   */
  async function send(request: Request) {
    if (!(request instanceof Request)) throw new TypeError("Foreign HTTP request");
    const body = request.take() as GuestBody;
    const method = request.methodName;
    const timeouts = Object.freeze({ ...request.options });
    const target = request.target();
    // A refusal before any provider call releases the unread body, so the guest's writers finish
    // and the refusal stays a recoverable `err`.
    if (target.tag === "err") {
      body.settle(err(target.val));
      body.discard();
      return target;
    }
    const provider = client?.provider(target.val.url);
    if (!provider) {
      body.settle(err({ tag: "HTTP-request-denied" } as ErrorCode));
      body.discard();
      return err({ tag: "HTTP-request-denied" } as ErrorCode);
    }
    const declared = request.headers.get("content-length")[0];
    const upload = body.upload(declared && BigInt(digits(declared)));
    const exchange = new AbortController();
    const outgoing: HttpProviderRequest = Object.freeze({
      url: target.val.url,
      pathWithQuery: target.val.pathWithQuery,
      method,
      fields: request.headers.pairs(),
      body: upload.body,
      trailers: upload.trailers,
      timeouts,
      signal: exchange.signal,
    });
    const fail = (code: ErrorCode) => {
      exchange.abort();
      body.settle(err(code));
      upload.abort(new HttpProviderError(code as never));
      return err(code);
    };
    // A body failure ends the exchange; an early stop by the provider leaves it to the provider.
    upload.completed.catch((error) => fail(errorCode(error)));
    const signal = host.signal();
    if (signal.aborted) return fail(internal("request was cancelled"));
    let pending!: Promise<HttpProviderResponse>;
    let response: HttpProviderResponse;
    try {
      response = await deadline(
        host.physical(() => {
          if (signal.aborted) return Promise.reject(cancelled());
          return (pending = provider.send(outgoing));
        }),
        signal,
        millis(timeouts.firstByte),
      );
    } catch (error) {
      // The provider's late result still belongs to the revision, although the caller stopped
      // waiting. Release its body when it arrives instead of leaking the unread connection.
      pending?.then(discard, () => {});
      if (signal.aborted) {
        // The caller observes cancellation separately. Let the guest finish this standard
        // result import cooperatively; throwing here would turn cancellation into a trap.
        return fail(internal("request was cancelled"));
      }
      return fail(errorCode(error));
    }
    let fields: HttpFields;
    let length: bigint | undefined;
    let reader: ReadableStreamDefaultReader<Uint8Array> | undefined;
    try {
      const status = response?.status;
      if (!Number.isInteger(status) || status < 100 || status > 999)
        throw new HttpProviderError(
          internal("the HTTP provider returned an invalid status") as never,
        );
      fields = HttpFields.received(response.fields);
      length = framedLength(method, status, fields);
      if (response.body !== null && !(response.body instanceof ReadableStream))
        throw new HttpProviderError(
          internal("the HTTP provider returned an invalid body") as never,
        );
      reader = response.body?.getReader();
    } catch (error) {
      discard(response);
      return fail(errorCode(error));
    }
    // After a response, the transmission succeeded once the body ended or the provider stopped
    // reading it (upstream: a dropped body sender is success).
    upload.completed.then(
      () => body.settle(done),
      () => {},
    );
    const received = new ReceivedBody(
      reader,
      response.trailers,
      length,
      millis(timeouts.betweenBytes),
      exchange,
      upload.abort,
    );
    return ok(new Response(fields, received, response.status));
  }

  /**
   * An incoming request's contents, pulled from the application's iterable only as the handler
   * reads. The iterator's `return()` runs once, as tracked cleanup, when the handler drops the
   * body early, when the exchange ends early, or when the call ends with the body unfinished.
   */
  class IncomingBody implements Body {
    #iterator: AsyncIterator<Uint8Array> | undefined;
    #stopped = false;
    #finished = false;
    #terminated: ErrorCode | undefined;
    readonly #stop = new AbortController();
    /** A response delivery is reading; the call's end then waits for that delivery. */
    delivering = false;
    /** The handler call ended. */
    callEnded = false;

    constructor(
      readonly source: AsyncIterable<Uint8Array> | null,
      readonly trailers: PromiseLike<readonly HttpField[] | undefined> | undefined,
    ) {
      if (typeof trailers?.then === "function") Promise.resolve(trailers).catch(() => {});
    }

    consume(res: unknown): [object, Promise<Trailers>] {
      // The application has no use for the handler's processing result.
      readFuture<unknown>(res, host.retired).catch(() => {});
      let settle!: (value: Trailers) => void;
      const trailers = new Promise<Trailers>((resolve) => (settle = resolve));
      const stream = hostBytes(
        () => this.#read(),
        (end, failure) => {
          if (end !== "eof") {
            this.#release();
            settle(end === "failed" ? err(errorCode(failure)) : ok(undefined));
            return;
          }
          this.#finished = true;
          if (!this.trailers) return settle(ok(undefined));
          Promise.resolve(this.trailers).then(
            (pairs) => {
              try {
                settle(ok(pairs?.length ? HttpFields.received(pairs) : undefined));
              } catch (error) {
                settle(err(errorCode(error)));
              }
            },
            (error) => settle(err(errorCode(error))),
          );
        },
        host.retired,
      );
      return [stream, trailers];
    }

    /** The next non-empty chunk, or `undefined` at the end; a terminated body fails the read. */
    async #read(): Promise<Uint8Array | undefined> {
      if (this.#terminated) throw new HttpProviderError(this.#terminated as never);
      if (!this.source || this.#stopped) return undefined;
      this.#iterator ??= this.source[Symbol.asyncIterator]();
      for (;;) {
        let next: IteratorResult<Uint8Array>;
        try {
          next = await abortable(this.#iterator.next(), this.#stop.signal);
        } catch (error) {
          if (this.#terminated) throw new HttpProviderError(this.#terminated as never);
          throw error;
        }
        if (next.done) {
          this.#iterator = undefined;
          return undefined;
        }
        if (!(next.value instanceof Uint8Array))
          throw new HttpProviderError(
            internal("incoming request chunks must be Uint8Array") as never,
          );
        if (next.value.byteLength) return next.value;
      }
    }

    /**
     * Stop the application's iterable once. Its factory, `return` lookup and call are guarded,
     * but its completion belongs to the application: neither the call nor retirement waits on
     * an application iterator whose `return` promise never settles.
     */
    #release(): void {
      if (this.#stopped) return;
      this.#stopped = true;
      const iterator = this.#iterator;
      const source = this.source;
      this.#iterator = undefined;
      if (iterator || source)
        // The contract calls the application's `return()` without awaiting it in the call.
        void Promise.resolve()
          .then(() => (iterator ?? source![Symbol.asyncIterator]()).return?.())
          .catch(() => {});
    }

    /** The exchange ended: a pending or later read fails with `code`. */
    terminate(code: ErrorCode): void {
      if (this.#terminated || this.#finished) return this.#release();
      this.#terminated = code;
      this.#stop.abort();
      this.#release();
    }

    /** Dropped unconsumed. */
    close(): void {
      this.#release();
    }
  }

  /** Received fields of the application; an invalid name or value is its error. */
  const incomingFields = (pairs: readonly HttpField[], what: string) => {
    try {
      return HttpFields.received(pairs);
    } catch {
      throw new TypeError(`invalid incoming ${what} field`);
    }
  };

  /** Request bodies issued by this instance, by the exchange's request. */
  const incoming = new WeakMap<IncomingHttpRequest, IncomingBody>();

  /**
   * The response the handler published, read by the application. The delivery ends once:
   * - end of body and trailers: the transmission result is `ok`, or the trailers' own error;
   * - the application stopped reading (`return()`): its client disconnected; the handler sees
   *   `connection-terminated`, its request body is closed and its next write fails;
   * - the revision retired: same signals, and a pending or later read rejects with the reason.
   */
  function deliver(published: Response, exchange: IngressExchange): Promise<void> {
    const status = published.getStatusCode();
    const pairs = published.headers.pairs();
    const body = published.take();
    // The response's own reference moved here; its body lives on alone.
    published.release();
    const request = incoming.get(exchange.request);
    if (request) request.delivering = true;
    let transmitted!: (outcome: Outcome) => void;
    const transmission = new Promise<Outcome>((resolve) => (transmitted = resolve));
    const [contents, future] = body.consume(transmission);
    const chunks: AsyncIterator<Uint8Array> =
      typeof (contents as { read?: unknown }).read === "function"
        ? guestBytes(contents, host.retired)
        : (contents as AsyncIterable<Uint8Array>)[Symbol.asyncIterator]();
    const written = future.then((result) => {
      if (result.tag === "err") throw new HttpHandlerError(result.val);
      const fields = result.val?.pairs();
      result.val?.release();
      return fields?.length ? fields : undefined;
    });
    let stopTrailers!: (reason: unknown) => void;
    const stopped = new Promise<never>((_, reject) => (stopTrailers = reject));
    const trailers = Promise.race([written, stopped]);
    for (const promise of [written, stopped, trailers]) promise.catch(() => {});
    let resolveEnd!: () => void;
    let rejectEnd!: (error: unknown) => void;
    const ended = new Promise<void>((resolve, reject) => {
      resolveEnd = resolve;
      rejectEnd = reject;
    });
    ended.catch(() => {});
    let finished = false;
    let started = false;
    let failure: unknown;
    const finish = (outcome: Outcome, error?: unknown) => {
      if (finished) return;
      finished = true;
      signal.removeEventListener("abort", retire);
      transmitted(outcome);
      if (request) {
        request.delivering = false;
        if (outcome.tag === "err" || request.callEnded) request.terminate(CONNECTION_TERMINATED);
      }
      if (error === undefined) resolveEnd();
      else rejectEnd(error);
    };
    /** The application stopped reading, or the revision retired with `reason`. */
    const disconnect = (reason?: unknown) => {
      if (finished) return;
      failure = reason;
      void chunks.return?.();
      stopTrailers(reason ?? new HttpHandlerError(CONNECTION_TERMINATED));
      finish(err(CONNECTION_TERMINATED), reason);
    };
    // The delivery is physical work of the handler's callback: the callback (its admission, its
    // scope and its deadline) lasts until the delivery ended, since the guest may still produce
    // the body after its export task returned. Cancellation, the deadline and retirement abort it.
    const signal = host.signal();
    const retire = () => disconnect(cancelled());
    if (signal.aborted) queueMicrotask(retire);
    else signal.addEventListener("abort", retire, { once: true });
    host.physical(() => ended).catch(() => {});
    const next = async (): Promise<IteratorResult<Uint8Array>> => {
      if (finished) {
        if (failure !== undefined) throw failure;
        return { done: true, value: undefined };
      }
      let chunk: IteratorResult<Uint8Array>;
      try {
        chunk = await chunks.next();
      } catch (error) {
        if (finished) throw failure ?? error;
        finish(err(errorCode(error)), error);
        throw error;
      }
      // A delivery that ended meanwhile never reports a clean end of body for it.
      if (finished) {
        if (failure !== undefined) throw failure;
        return { done: true, value: undefined };
      }
      if (!chunk.done) return { done: false, value: chunk.value };
      // The body ended; the transmission completes with the trailers.
      try {
        await trailers;
      } catch (error) {
        if (finished) throw failure ?? error;
        finish(err(errorCode(error)), error);
        throw error;
      }
      if (finished) {
        if (failure !== undefined) throw failure;
        return { done: true, value: undefined };
      }
      finish(done);
      return { done: true, value: undefined };
    };
    exchange.respond(
      Object.freeze({
        status,
        fields: Object.freeze(pairs.map((pair) => Object.freeze(pair) as HttpField)),
        body: Object.freeze({
          [Symbol.asyncIterator]: (): AsyncIterator<Uint8Array> => {
            if (started) throw new TypeError("the response body is read once");
            started = true;
            return {
              next,
              return: async () => {
                disconnect();
                return { done: true, value: undefined };
              },
            };
          },
        }),
        trailers,
      }) satisfies HttpHandlerResponse,
    );
    return ended;
  }

  const ingress: IngressBridge = {
    request(parts: IncomingHttpRequest) {
      const body = new IncomingBody(parts.body, parts.trailers);
      let headers: HttpFields;
      try {
        headers = incomingFields(parts.fields, "request");
      } catch (error) {
        body.close();
        throw error;
      }
      const request = new Request(headers, undefined, body);
      const refuse = (what: string) => {
        request.release();
        return new TypeError(`invalid incoming request ${what}`);
      };
      if (request.setMethod({ tag: "other", val: parts.method }).tag === "err")
        throw refuse("method");
      if (
        parts.scheme !== undefined &&
        request.setScheme({ tag: "other", val: parts.scheme }).tag === "err"
      )
        throw refuse("scheme");
      if (request.setAuthority(parts.authority).tag === "err") throw refuse("authority");
      if (request.setPathWithQuery(parts.pathWithQuery).tag === "err")
        throw refuse("path with query");
      incoming.set(parts, body);
      // The call's end closes an unfinished request body, unless a delivery still streams
      // (it may be passing that body through); the delivery's own end closes it then.
      const ended = host.signal();
      const end = () => {
        body.callEnded = true;
        if (!body.delivering) body.terminate(CONNECTION_TERMINATED);
      };
      if (ended.aborted) end();
      else ended.addEventListener("abort", end, { once: true });
      return request;
    },
    respond(response: unknown, exchange: IngressExchange) {
      if (!(response instanceof Response))
        return Promise.reject(new TypeError("Foreign HTTP response"));
      return deliver(response, exchange);
    },
  };

  return {
    types: {
      Fields: HttpFields,
      RequestOptions: HttpRequestOptions,
      Request,
      Response,
    },
    client: { send },
    ingress,
  };
}
