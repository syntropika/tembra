/**
 * The host transport contract behind standard `wasi:http`, independent of a platform adapter.
 *
 * An `HttpProvider` receives one grant-checked request with raw byte fields, a pull-based body
 * stream and a trailers promise, and returns raw response fields, a body stream and optional
 * trailers. Mocks and alternate transports implement the full WASI semantics through it
 * (duplicate raw fields, trailers both ways, uploads that stream while the response is read).
 * The browser Fetch adapter is implemented separately in `fetch-http-provider.ts`.
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

/** Field values are byte strings; Fetch carries them as one UTF-16 unit per byte. */
export function byteString(bytes: Uint8Array): string {
  let text = "";
  for (let at = 0; at < bytes.length; at += 8192)
    text += String.fromCharCode(...bytes.subarray(at, at + 8192));
  return text;
}

export function bytesOf(text: string): Uint8Array {
  return Uint8Array.from(text, (unit) => unit.charCodeAt(0) & 0xff);
}
