/**
 * Incoming HTTP for `Host.handleHttp`: the raw request and response shapes, the handler's error
 * and the private exchange that carries one request into an Orb's `wasi:http/handler` export.
 *
 * The raw shapes are lossless: field values are bytes, order and duplicates are kept, the
 * request target is passed verbatim, and both sides carry trailers. Bodies are
 * `AsyncIterable<Uint8Array>`, so nothing here needs WHATWG streams; `return()` on the response
 * body's iterator is the client disconnect. Nothing in this module performs I/O.
 */
import type { ErrorCode } from "wasi:http/types@0.3.0";
import { Errors } from "../errors.ts";
import { TembraError } from "../error.ts";
import type { HttpField } from "./http-provider.ts";

/** The exact standard export an Orb serves incoming HTTP through. */
export const HTTP_HANDLER = "wasi:http/handler@0.3.0";

/** One request as the host received it; the application owns listening, routing and auth. */
export interface IncomingHttpRequest {
  /** `GET`, `POST`, … or an extension token, verbatim. */
  readonly method: string;
  /** `http`, `https` or another scheme; absent when the request has none. */
  readonly scheme?: string | undefined;
  readonly authority?: string | undefined;
  /** The origin-form path and query exactly as received. */
  readonly pathWithQuery?: string | undefined;
  /** Lowercase names and raw value bytes, in order, duplicates kept. */
  readonly fields: readonly HttpField[];
  /**
   * The request contents, pulled as the handler reads, or `null` for none. The host calls its
   * iterator's `return()` when the handler drops the body early, the client disconnects or the
   * call ends.
   */
  readonly body: AsyncIterable<Uint8Array> | null;
  /** Trailers, read after the body ended; absent means none. */
  readonly trailers?: PromiseLike<readonly HttpField[] | undefined> | undefined;
}

/** The handler's response; the head arrives before the body is produced. */
export interface HttpHandlerResponse {
  /** 100..=599. */
  readonly status: number;
  readonly fields: readonly HttpField[];
  /**
   * The contents, produced while they are read; read once. Reading to the end and then the
   * trailers completes the transmission. Calling `return()` on its iterator (or abandoning a
   * `for await` loop) is the client disconnect: the handler sees `connection-terminated`, its
   * request body is closed and its next write fails.
   */
  readonly body: AsyncIterable<Uint8Array>;
  /**
   * Settles after the body ended: `undefined` for none. Rejects with an `HttpHandlerError` when
   * the handler failed its body or trailers, or when the delivery ended early.
   */
  readonly trailers: Promise<readonly HttpField[] | undefined>;
}

export interface HandleHttpOptions {
  /** Cancels the call, as cancelling its root does; the response body then ends. */
  readonly signal?: AbortSignal | undefined;
  /** The principal of the root this request runs in; defaults to the host's. */
  readonly principal?: string | undefined;
}

/** The handler (or the delivery of its response) ended with a standard `error-code`. */
export class HttpHandlerError extends Error {
  constructor(
    readonly code: ErrorCode,
    message = `HTTP handler failed: ${code.tag}`,
  ) {
    super(message);
    this.name = "HttpHandlerError";
  }
}

/** The transmission result the handler sees when its client disconnected. */
export const CONNECTION_TERMINATED: ErrorCode = { tag: "connection-terminated" };

/** Exchanges created by `Host.handleHttp`; nothing else can enter the handler method. */
const issued = new WeakSet<object>();

/**
 * One request travelling into one handler call. The head resolves as soon as the handler
 * published its response; a failure before that is reported only once the call physically
 * settled.
 */
export class IngressExchange {
  readonly head: Promise<HttpHandlerResponse>;
  #resolve!: (response: HttpHandlerResponse) => void;
  #reject!: (error: unknown) => void;
  #claimed = false;
  #published = false;
  #refusal: unknown;

  constructor(readonly request: IncomingHttpRequest) {
    this.head = new Promise((resolve, reject) => {
      this.#resolve = resolve;
      this.#reject = reject;
    });
    issued.add(this);
  }

  /** Whether the handler's result (a response or its error) was published. */
  get published(): boolean {
    return this.#published;
  }

  /** The handler returned a response: release the head now. */
  respond(response: HttpHandlerResponse): void {
    if (this.#published) throw new Error("the response head was already published");
    this.#published = true;
    this.#resolve(response);
  }

  /** The handler returned its `error-code`: reported once the call settled. */
  refuse(code: ErrorCode): void {
    if (this.#published) throw new Error("the response head was already published");
    this.#published = true;
    this.#refusal = new HttpHandlerError(code);
  }

  /** The call physically settled; without a published response, report why. */
  settle(failure?: unknown): void {
    if (this.#refusal !== undefined) return this.#reject(this.#refusal);
    if (this.#published && this.#refusal === undefined) return;
    this.#reject(failure ?? new Error("the HTTP handler ended without a response"));
  }

  /** Take the exchange for the handler call; a host-local value enters exactly one call. */
  static claim(value: unknown): IngressExchange {
    if (typeof value !== "object" || value === null || !issued.has(value))
      throw new TembraError(
        Errors.Invalid.LocalValue,
        `${HTTP_HANDLER} is served only through Host.handleHttp`,
      );
    const exchange = value as IngressExchange;
    if (exchange.#claimed) throw new Error("the HTTP request was already handled");
    exchange.#claimed = true;
    return exchange;
  }
}

/** Check the raw request's shape before any work starts; semantics are checked by the host. */
export function checkedRequest(request: IncomingHttpRequest): IncomingHttpRequest {
  if (typeof request !== "object" || request === null)
    throw new TypeError("handleHttp requires a request");
  const { method, scheme, authority, pathWithQuery, fields, body, trailers } = request;
  if (typeof method !== "string") throw new TypeError("request method must be a string");
  for (const [name, value] of [
    ["scheme", scheme],
    ["authority", authority],
    ["pathWithQuery", pathWithQuery],
  ] as const)
    if (value !== undefined && typeof value !== "string")
      throw new TypeError(`request ${name} must be a string`);
  if (!Array.isArray(fields)) throw new TypeError("request fields must be an array");
  const copied = fields.map((field) => {
    const [name, value] = Array.isArray(field) ? field : [];
    if (typeof name !== "string" || !(value instanceof Uint8Array))
      throw new TypeError("request fields are [name, Uint8Array] pairs");
    return Object.freeze([name, value.slice()] as const);
  });
  if (body !== null && typeof body?.[Symbol.asyncIterator] !== "function")
    throw new TypeError("request body must be an AsyncIterable<Uint8Array> or null");
  if (trailers !== undefined && typeof trailers?.then !== "function")
    throw new TypeError("request trailers must be a promise");
  return Object.freeze({
    method,
    scheme,
    authority,
    pathWithQuery,
    fields: Object.freeze(copied),
    body,
    trailers,
  });
}
