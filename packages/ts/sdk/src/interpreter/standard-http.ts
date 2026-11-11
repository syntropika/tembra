/**
 * Transport lifetime of outgoing standard `wasi:http@0.3.0` in the browser: request and response
 * bodies, the upload handed to an `HttpProvider`, `client.send`, and the binding of every
 * generated `wasi:http` function. Value semantics (fields, options, targets) live in
 * `standard-http-values.ts`; the provider contract lives in `http-provider.ts` and its Fetch
 * adapter in `fetch-http-provider.ts`.
 *
 * Ownership: one exchange owns its `Upload` and its `ReceivedBody`. The host ends both itself,
 * whether or not the provider cooperates: a failed or cancelled send, a response released or
 * dropped before its end, or a failed response body aborts the upload (its input returns to the
 * revision budget and the transmission future settles); revision retirement discards whatever is
 * left. A clean end of the response body ends nothing else, so a duplex upload may still finish.
 *
 * Authority: `send` checks the exact origin grant (through `HttpClient`) before any provider
 * call; a refusal is `error-code.HTTP-request-denied`, never a trap.
 */
import type { ErrorCode, Result } from "wasi:http/types@0.3.0";
import {
  type HttpField,
  type HttpProvider,
  HttpProviderError,
  type HttpProviderRequest,
  type HttpProviderResponse,
} from "./http-provider.ts";
import { METHODS, TYPES } from "./standard.ts";
import { encode, type HandleCodec, Writer } from "./standard-codec.ts";
import {
  type Body,
  digits,
  done,
  err,
  HttpFields,
  HttpRequest,
  HttpRequestOptions,
  HttpResponse,
  internal,
  type Message,
  ok,
  type Outcome,
  type Trailers,
} from "./standard-http-values.ts";
import {
  bindResource,
  type ByteProducer,
  cancelled,
  type FutureReader,
  HostFuture,
  type Implementation,
  type InputStream,
  OutputStream,
  type StandardCall,
  type StandardServices,
  type StreamEnd,
} from "./standard-streams.ts";

const TYPES_INTERFACE = "wasi:http/types@0.3.0";
const CLIENT_INTERFACE = "wasi:http/client@0.3.0";

/** Grant-checked selection of the host transport. */
export interface HttpClient {
  /** The provider granted for exactly this URL's origin, or `undefined` to deny. */
  provider(url: URL): HttpProvider | undefined;
}

/** The `error-code` type, read from the generated `client.send` result. */
const ERROR_CODE = (() => {
  const send = METHODS.find((m) => m.interface === CLIENT_INTERFACE && m.name === "send");
  const result = send?.result === undefined ? undefined : TYPES[send.result];
  return result?.[0] === "result" ? (result[2] as number) : undefined;
})();
const NO_HANDLES: HandleCodec = {
  handle() {
    throw new Error("an error code carries no handles");
  },
  value() {
    throw new Error("an error code carries no handles");
  },
};

/** A provider-supplied code only when it is a valid `error-code` value. */
function checkedCode(code: unknown): ErrorCode | undefined {
  if (ERROR_CODE === undefined) return undefined;
  try {
    encode(new Writer(), ERROR_CODE, code, NO_HANDLES);
    return code as ErrorCode;
  } catch {
    return undefined;
  }
}

/** Map a failure to an `error-code`; `fallback` names the phase-specific generic code. */
function errorCode(error: unknown, fallback: (message: string) => ErrorCode = internal): ErrorCode {
  if (error instanceof HttpProviderError) {
    const code = checkedCode(error.code);
    if (code) return code;
  }
  return fallback(error instanceof Error ? error.message : String(error));
}

/** Nanoseconds to timer milliseconds; a duration beyond the timer range means no timer. */
function millis(duration: bigint | undefined): number | undefined {
  if (duration === undefined) return undefined;
  const ms = (duration + 999_999n) / 1_000_000n;
  return ms > 0x7fffffffn ? undefined : Number(ms);
}

// ---------------------------------------------------------------------------------------------
// guest bodies and uploads

const EMPTY: ByteProducer = { next: async () => undefined, close() {} };

/**
 * A body the guest builds: an optional input stream, a guest-written trailers future, and the
 * transmission result future returned by `new`, which settles exactly once.
 */
class GuestBody implements Body {
  #settle: ((outcome: Outcome) => void) | undefined;
  readonly result: HostFuture<Outcome>;

  constructor(
    readonly contents: InputStream | undefined,
    readonly trailers: FutureReader<Trailers>,
  ) {
    this.result = new HostFuture(
      new Promise<Outcome>((resolve) => {
        this.#settle = resolve;
      }),
    );
  }

  /** Settle the transmission result once. */
  settle(outcome: Outcome): void {
    const settle = this.#settle;
    this.#settle = undefined;
    settle?.(outcome);
  }

  consume(_host: StandardServices, res: FutureReader<Outcome>): [OutputStream, HostFuture] {
    const contents = this.contents;
    const stream = new OutputStream(
      contents ? { next: () => contents.read(), close: () => contents.cancel() } : EMPTY,
      () => {},
    );
    // The consumer reports its handling outcome as this body's transmission result.
    res.take().then((outcome) => this.settle(outcome));
    const trailers = this.trailers.take();
    return [stream, new HostFuture(trailers, () => this.trailers.close())];
  }

  /** Stop the producer side; nothing this body accepted is transmitted. */
  fail(code: ErrorCode): void {
    this.contents?.cancel();
    this.trailers.close();
    this.settle(err(code));
  }

  /** Upstream: a dropped guest body closes its inputs and its result reports success. */
  close(): void {
    this.contents?.cancel();
    this.trailers.close();
    this.settle(done);
  }
}

/**
 * One guest body handed to a provider. It has exactly one terminal state: `completed` (every
 * byte, a matching `Content-Length` and an `ok` trailers result were handed over, and the body
 * stream closed) or a failure (the body stream errored, `trailers` rejected and the guest inputs
 * stopped). A failed upload never closes its stream, so a provider cannot commit a truncated
 * body. Each pull takes the next accepted guest chunk: no read-ahead, no body-wide copy.
 */
class Upload {
  readonly body: ReadableStream<Uint8Array> | null;
  readonly trailers: Promise<readonly HttpField[] | undefined>;
  /** Resolves when completed; rejects with the failure. */
  readonly completed: Promise<void>;
  #terminal: "completed" | HttpProviderError | undefined;
  #sent = 0;
  #controller: ReadableStreamDefaultController<Uint8Array> | undefined;
  #complete!: (trailers: readonly HttpField[] | undefined) => void;
  #reject!: (error: HttpProviderError) => void;

  constructor(
    readonly source: GuestBody,
    /** A declared request `Content-Length`. */
    readonly length: bigint | undefined,
  ) {
    let resolveTrailers!: (trailers: readonly HttpField[] | undefined) => void;
    let rejectTrailers!: (error: HttpProviderError) => void;
    this.trailers = new Promise((resolve, reject) => {
      resolveTrailers = resolve;
      rejectTrailers = reject;
    });
    this.completed = new Promise((resolve, reject) => {
      this.#complete = (trailers) => {
        resolveTrailers(trailers);
        resolve();
      };
      this.#reject = (error) => {
        rejectTrailers(error);
        reject(error);
      };
    });
    // A provider that never reads them must not see unhandled rejections.
    this.trailers.catch(() => {});
    this.completed.catch(() => {});
    const contents = source.contents;
    if (!contents) {
      this.body = null;
      void this.#finish();
      return;
    }
    // Retirement or a consumer stop ends the upload even if the provider never pulls again.
    contents.stopped.addEventListener(
      "abort",
      () => this.abort(internal("the request body was discarded")),
      { once: true },
    );
    this.body = new ReadableStream<Uint8Array>(
      {
        start: (controller) => {
          this.#controller = controller;
        },
        pull: (controller) => this.#pull(contents, controller),
        cancel: (reason) =>
          this.#fail(
            reason instanceof HttpProviderError
              ? reason
              : new HttpProviderError(internal("the HTTP provider cancelled the request body")),
          ),
      },
      { highWaterMark: 0 },
    );
  }

  /** Whether the upload reached its terminal state. */
  get ended(): boolean {
    return this.#terminal !== undefined;
  }

  /** Fail the upload unless it already ended. */
  abort(code: ErrorCode): void {
    this.#fail(new HttpProviderError(code));
  }

  #fail(error: HttpProviderError): void {
    if (this.#terminal) return;
    this.#terminal = error;
    this.source.fail(error.code);
    this.#reject(error);
    this.#controller?.error(error);
  }

  #oversized(): HttpProviderError {
    return new HttpProviderError({ tag: "HTTP-request-body-size", val: BigInt(this.#sent) });
  }

  async #pull(
    contents: InputStream,
    controller: ReadableStreamDefaultController<Uint8Array>,
  ): Promise<void> {
    const chunk = await contents.read();
    if (this.#terminal) return;
    if (!chunk) {
      if (await this.#finish()) controller.close();
      return;
    }
    this.#sent += chunk.byteLength;
    if (this.length !== undefined && BigInt(this.#sent) > this.length)
      return this.#fail(this.#oversized());
    controller.enqueue(chunk);
  }

  /** After the last chunk: check the length, then wait for the guest's trailers result. */
  async #finish(): Promise<boolean> {
    if (this.length !== undefined && BigInt(this.#sent) !== this.length) {
      this.#fail(this.#oversized());
      return false;
    }
    let result: Trailers;
    try {
      result = await this.source.trailers.take();
    } catch (error) {
      this.#fail(new HttpProviderError(errorCode(error)));
      return false;
    }
    if (this.#terminal) return false;
    if (result.tag === "err") {
      this.#fail(new HttpProviderError(result.val));
      return false;
    }
    const fields = result.val?.pairs();
    result.val?.release();
    this.#terminal = "completed";
    this.#complete(fields);
    return true;
  }
}

// ---------------------------------------------------------------------------------------------
// received bodies

/**
 * A response body received from a provider, revision-owned once published: reads may span
 * callbacks. It ends in exactly one of three ways, given by the stream's explicit end reason:
 * - `eof`: the body ended with a matching `Content-Length`; trailers are the provider's, and
 *   nothing else is aborted (a duplex upload may still finish);
 * - `dropped` (or the response released unconsumed): trailers are `none`, as upstream's
 *   `HostBodyStreamProducer` drop reports, and the exchange is released;
 * - `failed`: trailers are the error, and the exchange is released.
 * Releasing aborts the provider's signal, aborts an unfinished upload and cancels the body; the
 * cancel promise and any read still in flight are tracked as physical cleanup.
 */
class ReceivedBody implements Body {
  #released = false;
  #received = 0;
  #reading: Promise<unknown> | undefined;

  constructor(
    readonly host: StandardServices,
    readonly signal: AbortController,
    readonly upload: Upload,
    readonly reader: ReadableStreamDefaultReader<Uint8Array> | undefined,
    readonly trailers: Promise<readonly HttpField[] | undefined> | undefined,
    /** The declared response `Content-Length`, when the response has a framed body. */
    readonly length: bigint | undefined,
    readonly betweenBytes: number | undefined,
  ) {
    trailers?.catch(() => {});
  }

  consume(_host: StandardServices, res: FutureReader<Outcome>): [OutputStream, HostFuture] {
    // The transport has no use for the consumer's handling outcome.
    res.close();
    let settle!: (trailers: Trailers) => void;
    const trailers = new Promise<Trailers>((resolve) => {
      settle = resolve;
    });
    const stream = new OutputStream(
      { next: () => this.#read(), close: (end) => this.#end(end) },
      (failure, end) => {
        if (end === "failed")
          return settle(err(errorCode(failure, () => ({ tag: "HTTP-response-incomplete" }))));
        if (end === "dropped" || !this.trailers) return settle(ok(undefined));
        this.trailers.then(
          (fields) => {
            try {
              settle(ok(fields ? HttpFields.received(fields) : undefined));
            } catch (error) {
              settle(err(errorCode(error)));
            }
          },
          (error) => settle(err(errorCode(error))),
        );
      },
    );
    return [stream, new HostFuture(trailers)];
  }

  /** The next non-empty chunk; empty chunks are legal and are not the end of the body. */
  async #read(): Promise<Uint8Array | undefined> {
    for (;;) {
      const next = this.reader ? await this.#next(this.reader) : undefined;
      if (!next || next.done) {
        if (this.length !== undefined && BigInt(this.#received) !== this.length)
          throw new HttpProviderError({ tag: "HTTP-response-incomplete" });
        return undefined;
      }
      const chunk: unknown = next.value;
      if (!(chunk instanceof Uint8Array))
        throw new HttpProviderError(internal("the HTTP provider produced a non-byte chunk"));
      if (!chunk.byteLength) continue;
      this.#received += chunk.byteLength;
      if (this.length !== undefined && BigInt(this.#received) > this.length)
        throw new HttpProviderError({
          tag: "HTTP-response-body-size",
          val: BigInt(this.#received),
        });
      return chunk;
    }
  }

  /** One provider read under the between-bytes deadline, enforced whatever the provider does. */
  async #next(
    reader: ReadableStreamDefaultReader<Uint8Array>,
  ): ReturnType<ReadableStreamDefaultReader<Uint8Array>["read"]> {
    const read = reader.read();
    this.#reading = read.catch(() => {});
    if (this.betweenBytes === undefined) return read;
    let timer: ReturnType<typeof setTimeout> | undefined;
    try {
      return await Promise.race([
        read,
        new Promise<never>((_, reject) => {
          timer = setTimeout(
            () => reject(new HttpProviderError({ tag: "connection-read-timeout" })),
            this.betweenBytes,
          );
        }),
      ]);
    } finally {
      clearTimeout(timer);
    }
  }

  /** The guest stream ended; only an early drop or a failure releases the exchange. */
  #end(end: StreamEnd = "dropped"): void {
    if (end !== "eof") this.close();
  }

  /** Release the exchange: the response was dropped early, failed, or was released unread. */
  close(): void {
    if (this.#released) return;
    this.#released = true;
    this.signal.abort();
    this.upload.abort(internal("the response was released before its request body completed"));
    const reader = this.reader;
    const reading = this.#reading;
    if (reader)
      this.host.cleanup(async () => {
        await reader.cancel();
        await reading;
      });
  }
}

// ---------------------------------------------------------------------------------------------
// client.send

/** Cancel a response nobody will receive, tracking the provider's cancellation. */
function discard(host: StandardServices, response: unknown): void {
  const body = (response as Partial<HttpProviderResponse> | undefined)?.body;
  if (body instanceof ReadableStream) host.cleanup(() => body.cancel());
}

/**
 * Wait for the provider's response until the callback is cancelled or the first-byte deadline
 * passes, whichever comes first. Neither depends on the provider honoring its signal.
 */
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
    if (signal?.aborted) return abort();
    signal?.addEventListener("abort", abort, { once: true });
    if (ms !== undefined)
      timer = setTimeout(() => {
        stop();
        reject(new HttpProviderError({ tag: "connection-read-timeout" }));
      }, ms);
    pending.then(
      (value) => {
        stop();
        resolve(value);
      },
      (error) => {
        stop();
        reject(error);
      },
    );
  });
}

/** A response's framed body length; `undefined` where HTTP defines no body or none was declared. */
function framedLength(method: string, status: number, fields: HttpFields): bigint | undefined {
  if (method === "HEAD" || status < 200 || status === 204 || status === 304) return undefined;
  const declared = new Set(fields.get("content-length").map(digits));
  if (declared.size > 1)
    throw new HttpProviderError(internal("the HTTP provider returned conflicting lengths"));
  const [length] = declared;
  return length === undefined ? undefined : BigInt(length);
}

/**
 * `wasi:http/client#send`. Ownership of the request moves at this first turn, so everything
 * the exchange needs from it is captured before the first `await`. The exact origin grant is
 * checked before any provider call. The response is published once its fields arrived, then
 * outlives the callback while the upload may still be streaming.
 */
async function send(
  call: StandardCall,
  client: HttpClient | undefined,
  request: HttpRequest,
): Promise<Result<HttpResponse, ErrorCode>> {
  const body = request.take() as GuestBody;
  const method = request.methodName;
  const timeouts = Object.freeze({ ...request.options });
  const target = request.target();
  if (target.tag === "err") {
    body.fail(target.val);
    return target;
  }
  const provider = client?.provider(target.val.url);
  if (!provider) {
    body.fail({ tag: "HTTP-request-denied" });
    return err({ tag: "HTTP-request-denied" });
  }
  const declared = request.headers.get("content-length")[0];
  const upload = new Upload(body, declared && BigInt(digits(declared)));
  const signal = new AbortController();
  const outgoing: HttpProviderRequest = Object.freeze({
    url: target.val.url,
    pathWithQuery: target.val.pathWithQuery,
    method,
    fields: request.headers.pairs(),
    body: upload.body,
    trailers: upload.trailers,
    timeouts,
    signal: signal.signal,
  });
  // A failed exchange ends the upload and settles the transmission future, even if the upload
  // itself had already completed.
  const fail = (code: ErrorCode) => {
    signal.abort();
    upload.abort(code);
    body.settle(err(code));
    return err(code);
  };
  const pending = call.host.physical(call.signal, () => provider.send(outgoing));
  let response: HttpProviderResponse;
  try {
    response = await deadline(pending, call.signal, millis(timeouts.firstByte));
  } catch (error) {
    // A late response belongs to nobody; its body is cancelled under physical accounting.
    pending.then(
      (late) => discard(call.host, late),
      () => {},
    );
    if (!call.signal?.aborted) return fail(errorCode(error));
    fail(internal("request was cancelled"));
    throw cancelled();
  }
  let fields: HttpFields;
  let length: bigint | undefined;
  let reader: ReadableStreamDefaultReader<Uint8Array> | undefined;
  try {
    const status = response?.status;
    if (!Number.isInteger(status) || status < 100 || status > 999)
      throw new HttpProviderError(internal("the HTTP provider returned an invalid status"));
    fields = HttpFields.received(response.fields);
    length = framedLength(method, status, fields);
    if (response.body !== null && !(response.body instanceof ReadableStream))
      throw new HttpProviderError(internal("the HTTP provider returned an invalid body"));
    reader = response.body?.getReader();
  } catch (error) {
    discard(call.host, response);
    return fail(errorCode(error));
  }
  // Transmitted: the response was accepted and, possibly later, the whole upload handed over.
  upload.completed.then(
    () => body.settle(done),
    () => {},
  );
  const received = new ReceivedBody(
    call.host,
    signal,
    upload,
    reader,
    response.trailers,
    length,
    millis(timeouts.betweenBytes),
  );
  return ok(new HttpResponse(fields, received, response.status));
}

// ---------------------------------------------------------------------------------------------
// binding

/** Every generated `wasi:http` function, keyed for the standard host. */
export function httpMethods(client: HttpClient | undefined): Record<string, Implementation> {
  const consume = (
    call: StandardCall,
    message: Message,
    res: FutureReader<Outcome>,
  ): [OutputStream, HostFuture] => message.take().consume(call.host, res);
  return {
    ...bindResource(TYPES_INTERFACE, "fields", HttpFields.prototype, {
      constructor: () => HttpFields.create(),
      fromList: (_call, entries: [string, Uint8Array][]) => HttpFields.fromList(entries),
    }),
    ...bindResource(TYPES_INTERFACE, "request-options", HttpRequestOptions.prototype, {
      constructor: () => new HttpRequestOptions(),
    }),
    ...bindResource(TYPES_INTERFACE, "request", HttpRequest.prototype, {
      new: (
        _call,
        headers: HttpFields,
        contents: InputStream | undefined,
        trailers: FutureReader<Trailers>,
        options: HttpRequestOptions | undefined,
      ) => {
        const body = new GuestBody(contents, trailers);
        return [new HttpRequest(headers.frozen(), options?.timeouts, body), body.result];
      },
      consumeBody: consume,
    }),
    ...bindResource(TYPES_INTERFACE, "response", HttpResponse.prototype, {
      new: (
        _call,
        headers: HttpFields,
        contents: InputStream | undefined,
        trailers: FutureReader<Trailers>,
      ) => {
        const body = new GuestBody(contents, trailers);
        return [new HttpResponse(headers.frozen(), body), body.result];
      },
      consumeBody: consume,
    }),
    [`${CLIENT_INTERFACE}#send`]: (call, [request]: [HttpRequest]) => send(call, client, request),
  };
}
