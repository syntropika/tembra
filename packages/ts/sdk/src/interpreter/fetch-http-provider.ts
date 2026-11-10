/** WHATWG Fetch implementation of the host HTTP contract, with explicit platform limits. */
import { byteString, bytesOf, HttpProviderError, type HttpProvider } from "./http-provider.ts";

/** The largest request body `fetchHttpProvider` buffers before calling Fetch. */
export const FETCH_BODY_BYTES = 8 << 20;
/** Methods Fetch refuses to send. */
const FETCH_FORBIDDEN_METHODS = new Set(["CONNECT", "TRACE", "TRACK"]);

const unsupported = (message: string) =>
  new HttpProviderError({ tag: "internal-error", val: message }, message);

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
