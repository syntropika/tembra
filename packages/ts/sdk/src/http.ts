import { encodeHost, parseURL } from "ufo";
import { send } from "wasi:http/client@0.3.0";
import {
  Fields,
  Request,
  RequestOptions,
  Response,
  type ErrorCode,
  type HeaderError,
  type RequestOptionsError,
  type Method,
  type Result,
} from "wasi:http/types@0.3.0";
import { readChunks, writeChunks, type WitFuture } from "./guest.ts";
import { decodeText, encodeText, Utf8Decoder } from "./http/bytes.ts";
import { events, type ServerSentEvent } from "./http/events.ts";

export type { ServerSentEvent } from "./http/events.ts";

export type HeaderInput =
  | Readonly<Record<string, string>>
  | Iterable<readonly [string, string | Uint8Array]>;

/** A snapshot of standard HTTP fields. Iteration and getAll preserve duplicates and bytes. */
export class HttpHeaders implements Iterable<readonly [string, Uint8Array]> {
  readonly #entries: readonly (readonly [string, Uint8Array])[];
  constructor(entries: Iterable<readonly [string, Uint8Array]> = []) {
    this.#entries = [...entries].map(
      ([name, bytes]) => [name.toLowerCase(), bytes.slice()] as const,
    );
  }
  get(name: string): string | undefined {
    const value = this.#entries.find(([key]) => key === name.toLowerCase())?.[1];
    return value === undefined ? undefined : new Utf8Decoder().decode(value, true);
  }
  getAll(name: string): readonly Uint8Array[] {
    return this.#entries
      .filter(([key]) => key === name.toLowerCase())
      .map(([, bytes]) => bytes.slice());
  }
  *[Symbol.iterator](): IterableIterator<readonly [string, Uint8Array]> {
    for (const [name, bytes] of this.#entries) yield [name, bytes.slice()];
  }
}

export interface HttpRequest {
  method?: string;
  headers?: HeaderInput;
  body?: string | Uint8Array | AsyncIterable<Uint8Array>;
  /** Optional trailing fields; native HTTP/1 also requires their names in the Trailer header. */
  trailers?: PromiseLike<HeaderInput | undefined>;
  /** Standard WASI durations, in nanoseconds. */
  timeouts?: { connect?: bigint; firstByte?: bigint; betweenBytes?: bigint };
}

/** Consume a response once. Call close when the body is no longer needed. */
export interface HttpResponse {
  readonly status: number;
  readonly ok: boolean;
  readonly headers: HttpHeaders;
  readonly body: AsyncIterable<Uint8Array>;
  text(): Promise<string>;
  json<T = unknown>(): Promise<T>;
  events(): AsyncIterable<ServerSentEvent>;
  /** Resolves after the response body has been consumed. */
  trailers(): Promise<HttpHeaders>;
  close(): Promise<void>;
}

/** A failed standard HTTP operation; its typed WIT error remains available as code. */
export class HttpError extends Error {
  constructor(readonly code: ErrorCode | HeaderError | RequestOptionsError | undefined) {
    super(`HTTP operation failed: ${code?.tag ?? "request rejected"}`);
    this.name = "HttpError";
  }
}

function fields(input: HeaderInput = {}): Fields {
  const entries =
    Symbol.iterator in input
      ? [...(input as Iterable<readonly [string, string | Uint8Array]>)]
      : Object.entries(input);
  return Fields.fromList(
    entries.map(([name, value]) => [name, typeof value === "string" ? encodeText(value) : value]),
  );
}

/** Release only still-owned values; an accepted standard call has already moved its inputs. */
function drop(value: unknown): void {
  try {
    (value as Disposable | undefined)?.[Symbol.dispose]?.();
  } catch {
    // Transferred resources no longer have a guest handle.
  }
}

function result<T>(value: Result<T, ErrorCode>): T {
  if (value.tag === "err") throw new HttpError(value.val);
  return value.val;
}

/** HTTP convenience over standard WASI imports. Network I/O and TLS belong to the host. */
export async function fetch(url: string, options: HttpRequest = {}): Promise<HttpResponse> {
  const parsed = parseURL(url);
  const scheme = parsed.protocol?.toLowerCase();
  if ((scheme !== "http:" && scheme !== "https:") || !parsed.host || parsed.auth)
    throw new TypeError("HTTP requires an absolute http or https URL without credentials");
  let method = options.method ?? "GET";
  const standard = ["DELETE", "GET", "HEAD", "OPTIONS", "POST", "PUT", "PATCH"];
  if (standard.includes(method.toUpperCase())) method = method.toUpperCase();
  if (["CONNECT", "TRACE", "TRACK"].includes(method.toUpperCase()))
    throw new TypeError(`HTTP method ${method} is forbidden`);
  if ((method === "GET" || method === "HEAD") && options.body !== undefined)
    throw new TypeError(`${method} requests cannot have a body`);
  if (
    options.body !== undefined &&
    typeof options.body !== "string" &&
    !(options.body instanceof Uint8Array) &&
    typeof options.body?.[Symbol.asyncIterator] !== "function"
  )
    throw new TypeError("HTTP body requires text, bytes or an AsyncIterable<Uint8Array>");

  if (
    [...parsed.host].some(
      (character) => character.charCodeAt(0) <= 32 || character.charCodeAt(0) === 127,
    )
  )
    throw new TypeError("HTTP URL host contains whitespace or controls");
  let headers: Fields | undefined;
  let timeouts: RequestOptions | undefined;
  let request: Request | undefined;
  let upload: WitFuture<Result<void, ErrorCode>> | undefined;
  try {
    headers = fields(options.headers);
    if (options.timeouts) {
      timeouts = new RequestOptions();
      timeouts.setConnectTimeout(options.timeouts.connect);
      timeouts.setFirstByteTimeout(options.timeouts.firstByte);
      timeouts.setBetweenBytesTimeout(options.timeouts.betweenBytes);
    }
    const body =
      typeof options.body === "string"
        ? [encodeText(options.body)]
        : options.body instanceof Uint8Array
          ? [options.body.slice()]
          : options.body;
    const trailers: Promise<Result<Fields | undefined, ErrorCode>> = Promise.resolve(
      options.trailers,
    ).then(
      (value) => ({ tag: "ok", val: value ? fields(value) : undefined }),
      (error) => ({ tag: "err", val: { tag: "internal-error", val: String(error) } }),
    );
    const [created, done] = Request.new(
      headers,
      body === undefined ? undefined : writeChunks(body),
      trailers,
      timeouts,
    );
    request = created;
    // Keep the standard future lazy and revision-owned. Reading it here would bind its
    // observation to this callback and break a response consumed in a later callback.
    upload = done;
    request.setMethod(
      standard.includes(method)
        ? ({ tag: method.toLowerCase() } as Method)
        : { tag: "other", val: method },
    );
    request.setScheme({ tag: scheme === "https:" ? "HTTPS" : "HTTP" });
    request.setAuthority(encodeHost(parsed.host));
    request.setPathWithQuery((parsed.pathname || "/") + (parsed.search || ""));
    const pending = send(request);
    request = undefined;
    return response(await pending, upload);
  } catch (error) {
    drop(request);
    drop(upload);
    // A standard resource/future error preserves its payload; transport traps remain traps.
    if (error && typeof error === "object" && "payload" in error)
      throw new HttpError(error.payload as HttpError["code"]);
    throw error;
  } finally {
    drop(headers);
    drop(timeouts);
  }
}

/** Snapshot child fields before transferring the response to its body stream. */
function response(value: Response, upload: WitFuture<Result<void, ErrorCode>>): HttpResponse {
  const status = value.getStatusCode();
  const fields = value.getHeaders();
  let headers: HttpHeaders;
  try {
    headers = new HttpHeaders(fields.copyAll());
  } finally {
    drop(fields);
  }
  // The handling result is created only when the body is consumed. A pending promise created
  // here would belong to the callback that received the response, so a response retained
  // across callbacks would leave its first callback with unsettled work.
  let finish: ((result: Result<void, ErrorCode>) => void) | undefined;
  let stream: AsyncIterable<Uint8Array> | undefined;
  let complete: WitFuture<Result<Fields | undefined, ErrorCode>> | undefined;
  let consumed = false;
  let ended = false;
  let closed = false;
  let trailing: Promise<HttpHeaders> | undefined;
  const trailers = (): Promise<HttpHeaders> => {
    if (!complete)
      return Promise.reject(new Error("Consume the HTTP body before reading trailers"));
    // A lifted future is a handle, not a thenable: `read()` returns its value once.
    return (trailing ??= complete.read().then((state) => {
      const fields = result(state);
      try {
        return new HttpHeaders(fields?.copyAll());
      } finally {
        drop(fields);
      }
    }));
  };
  const close = async () => {
    if (closed) return;
    closed = true;
    drop(upload);
    if (!ended) {
      finish?.({ tag: "err", val: { tag: "internal-error", val: "response closed" } });
      if (stream) {
        drop(stream);
        drop(complete);
      } else drop(value);
    }
  };
  const body: AsyncIterable<Uint8Array> = {
    async *[Symbol.asyncIterator]() {
      if (consumed) throw new Error("HTTP body has already been consumed");
      if (closed) throw new Error("HTTP response is closed");
      consumed = true;
      try {
        // Keep an untouched response revision-owned across callbacks. The handling future
        // belongs to the callback that actually consumes its body.
        const handled = new Promise<Result<void, ErrorCode>>((resolve) => {
          finish = resolve;
        });
        [stream, complete] = Response.consumeBody(value, handled);
        yield* readChunks(stream);
        finish!({ tag: "ok", val: undefined });
        await trailers();
        result(await upload.read());
        ended = true;
      } finally {
        await close();
      }
    },
  };
  return Object.freeze({
    status,
    ok: status >= 200 && status < 300,
    headers,
    body,
    text: () => decodeText(body, 1 << 20),
    json: async <T>() => JSON.parse(await decodeText(body, 1 << 20)) as T,
    events: async function* () {
      try {
        if (
          headers.get("content-type")?.split(";", 1)[0]?.trim().toLowerCase() !==
          "text/event-stream"
        )
          throw new Error("SSE requires text/event-stream");
        yield* events(body);
      } finally {
        await close();
      }
    },
    trailers,
    close,
  });
}
