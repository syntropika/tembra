/**
 * The host transport contract behind standard `wasi:http` in the browser, and its default
 * adapter over WHATWG Fetch.
 *
 * An `HttpProvider` receives one grant-checked request with raw byte fields, a pull-based body
 * stream and a trailers promise, and returns raw response fields, a body stream and optional
 * trailers. Mocks and alternate transports implement the full WASI semantics through it
 * (duplicate raw fields, trailers both ways, uploads that stream while the response is read).
 * `fetchHttpProvider` adapts Fetch to the same contract and isolates Fetch's platform limits.
 */
import type { ErrorCode } from "wasi:http/types@0.3.0";

/** One field: a lowercase name and its raw value bytes. Order and duplicates are significant. */
export type HttpField = readonly [name: string, value: Uint8Array];

/** WASI `request-options` durations in nanoseconds; absent means no timeout was requested. */
export interface HttpTimeouts {
  readonly connect?: bigint | undefined;
  readonly firstByte?: bigint | undefined;
  readonly betweenBytes?: bigint | undefined;
}

/**
 * One outgoing request. The host enforces `firstByte` (until the response promise settles) and
 * `betweenBytes` (per response body read) for every provider; `connect` is the provider's to
 * honor or refuse.
 */
export interface HttpProviderRequest {
  /**
   * The granted canonical origin with the WHATWG-normalized path, for URL-based transports
   * such as Fetch (which resolve dot segments and backslashes and escape some characters).
   */
  readonly url: URL;
  /** The request target exactly as the guest validated it (origin-form path and query). */
  readonly pathWithQuery: string;
  /** `GET`, `POST`, … or an extension token verbatim. */
  readonly method: string;
  readonly fields: readonly HttpField[];
  /**
   * The request contents, pulled one bounded chunk at a time from the guest (no read-ahead), or
   * `null` when the request has none. It closes only after the whole upload, including a
   * declared `Content-Length` and the trailers result, succeeded; a failed or abandoned upload
   * errors it with an `HttpProviderError` and never closes it, so a provider cannot commit a
   * truncated body. A provider that will not read it must cancel it; the host ends it
   * anyway when the response is released early or fails, or the revision retires. Cancelling
   * it stops the guest's upload.
   */
  readonly body: ReadableStream<Uint8Array> | null;
  /**
   * Trailers, settled once the body ended: `undefined` for none. Rejects with the upload's
   * `HttpProviderError` when the upload fails.
   */
  readonly trailers: Promise<readonly HttpField[] | undefined>;
  readonly timeouts: HttpTimeouts;
  /**
   * Aborts when the originating callback is cancelled or the first-byte timeout expires before
   * the response promise settled, and when the host releases the response early (dropped before
   * its end, a body failure, or retirement before its end). A cleanly ended response body does
   * not abort it. Retirement still errors any unfinished upload body and its trailers.
   */
  readonly signal: AbortSignal;
}

/** One response, revision-owned by the host once returned. */
export interface HttpProviderResponse {
  /** 100..=999. */
  readonly status: number;
  readonly fields: readonly HttpField[];
  /**
   * The response contents, or `null` for none. Empty chunks are allowed. The host reads it only
   * as the guest reads, enforces a declared `Content-Length` (except for `HEAD`, 1xx, 204 and
   * 304), and cancels it once when it releases the response early; that cancel promise is the
   * physical acknowledgement the host waits for.
   */
  readonly body: ReadableStream<Uint8Array> | null;
  /** Trailers, read after the body ended; absent means none. */
  readonly trailers?: Promise<readonly HttpField[] | undefined>;
}

/** A host HTTP transport. The promise is physical work: the host tracks it until it settles. */
export interface HttpProvider {
  send(request: HttpProviderRequest): Promise<HttpProviderResponse>;
}

/** A transport failure with a standard `error-code`; any other failure maps to `internal-error`. */
export class HttpProviderError extends Error {
  constructor(
    readonly code: ErrorCode,
    message = `HTTP provider failed: ${code.tag}`,
  ) {
    super(message);
    this.name = "HttpProviderError";
  }
}

/** The largest request body `fetchHttpProvider` buffers before calling Fetch. */
export const FETCH_BODY_BYTES = 8 << 20;
/** Methods Fetch refuses to send. */
const FETCH_FORBIDDEN_METHODS = new Set(["CONNECT", "TRACE", "TRACK"]);

const unsupported = (message: string) =>
  new HttpProviderError({ tag: "internal-error", val: message }, message);

/** Field values are byte strings; Fetch carries them as one UTF-16 unit per byte. */
function byteString(bytes: Uint8Array): string {
  let text = "";
  for (let at = 0; at < bytes.length; at += 8192)
    text += String.fromCharCode(...bytes.subarray(at, at + 8192));
  return text;
}

function bytesOf(text: string): Uint8Array {
  return Uint8Array.from(text, (unit) => unit.charCodeAt(0) & 0xff);
}

/** Fetch needs the whole upload before it starts; collect it under `FETCH_BODY_BYTES`. */
async function collect(body: ReadableStream<Uint8Array>): Promise<Uint8Array> {
  const reader = body.getReader();
  const chunks: Uint8Array[] = [];
  let total = 0;
  for (let next = await reader.read(); !next.done; next = await reader.read()) {
    total += next.value.byteLength;
    if (total > FETCH_BODY_BYTES) {
      const error = new HttpProviderError({ tag: "HTTP-request-body-size", val: BigInt(total) });
      // The reason becomes the upload's outcome.
      await reader.cancel(error);
      throw error;
    }
    chunks.push(next.value);
  }
  const bytes = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return bytes;
}

/**
 * Adapt Fetch (by default the global one) to `HttpProvider`. Fetch owns DNS, TLS and
 * connections. Its platform limits are explicit errors, never emulation:
 * - the upload is buffered (at most `FETCH_BODY_BYTES`) and sent after its trailers settled,
 *   so there is no duplex; non-empty request trailers are refused;
 * - responses have no trailers, and Fetch combines duplicate response fields;
 * - there is no separate connect phase: a `connect` timeout is refused;
 * - `redirect: "manual"` never follows a redirect, and the opaque result is refused;
 * - `CONNECT`/`TRACE`/`TRACK`, and a body with `GET` or `HEAD`, are refused;
 * - credentials are omitted, and browsers silently drop their forbidden request fields;
 * - Fetch sends `request.url`, whose path is WHATWG-normalized, not the raw `pathWithQuery`.
 */
export function fetchHttpProvider(
  fetch: (input: URL, init: RequestInit) => Promise<Response> = (input, init) =>
    globalThis.fetch(input, init),
): HttpProvider {
  return {
    async send(request) {
      if (FETCH_FORBIDDEN_METHODS.has(request.method.toUpperCase()))
        throw new HttpProviderError({ tag: "HTTP-request-method-invalid" });
      if (request.timeouts.connect !== undefined)
        throw unsupported("browser Fetch has no separate connect timeout");
      const body = request.body ? await collect(request.body) : new Uint8Array();
      if ((await request.trailers)?.length)
        throw unsupported("browser Fetch cannot send request trailers");
      if (body.length && (request.method === "GET" || request.method === "HEAD"))
        throw unsupported(`browser Fetch cannot send a body with ${request.method}`);
      const headers = new Headers();
      try {
        for (const [name, value] of request.fields) headers.append(name, byteString(value));
      } catch (error) {
        throw unsupported(error instanceof Error ? error.message : String(error));
      }
      let response: Response;
      try {
        response = await fetch(request.url, {
          method: request.method,
          headers,
          credentials: "omit",
          redirect: "manual",
          signal: request.signal,
          ...(body.length ? { body: body as Uint8Array<ArrayBuffer> } : {}),
        });
      } catch (error) {
        if (request.signal.aborted || error instanceof HttpProviderError) throw error;
        throw unsupported(error instanceof Error ? error.message : String(error));
      }
      if (response.type === "opaqueredirect" || response.redirected) {
        await response.body?.cancel().catch(() => {});
        throw unsupported(
          response.redirected
            ? "the Fetch implementation followed a redirect"
            : "browser Fetch exposes redirect responses only as opaque responses",
        );
      }
      return {
        status: response.status,
        fields: [...response.headers].map(([name, value]) => [name, bytesOf(value)] as const),
        body: response.body,
      };
    },
  };
}
