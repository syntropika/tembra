/**
 * The WHATWG `Request`/`Response` convenience form of `Host.handleHttp`, for hosts that have
 * those globals. It refuses what WHATWG cannot represent instead of dropping it:
 * - a 1xx status (the `Response` constructor accepts only 200..=599);
 * - a field the platform's `Response` would hide (browsers drop `set-cookie`);
 * - non-empty response trailers: the body errors at its end, after every byte was delivered.
 * Known WHATWG normalizations stay documented, not refused: duplicate fields are joined with
 * ", " (except `set-cookie`), values lose surrounding whitespace, the request URL is normalized
 * and a `Request` has no trailers. Use `IncomingHttpRequest` for the lossless raw form.
 */
import { byteString, bytesOf } from "./http-provider.ts";
import type { HttpHandlerResponse, IncomingHttpRequest } from "./http-ingress.ts";

/** Statuses whose `Response` carries no body. */
const NULL_BODY = new Set([101, 103, 204, 205, 304]);

/** Whether `value` is a WHATWG `Request` of this realm. */
export function isFetchRequest(value: unknown): value is Request {
  return typeof Request === "function" && value instanceof Request;
}

/** Read a WHATWG stream through an iterator whose `return()` cancels it. */
function iterate(stream: ReadableStream<Uint8Array>): AsyncIterable<Uint8Array> {
  return {
    [Symbol.asyncIterator]() {
      const reader = stream.getReader();
      return {
        async next() {
          const next = await reader.read();
          return next.done ? { done: true, value: undefined } : { done: false, value: next.value };
        },
        async return() {
          await reader.cancel();
          return { done: true, value: undefined };
        },
      };
    },
  };
}

/** The raw request a WHATWG `Request` describes. */
export function fromFetchRequest(request: Request): IncomingHttpRequest {
  const url = new URL(request.url);
  return {
    method: request.method,
    scheme: url.protocol.slice(0, -1),
    authority: url.host,
    pathWithQuery: url.pathname + url.search,
    fields: [...request.headers].map(([name, value]) => [name, bytesOf(value)] as const),
    body: request.body ? iterate(request.body) : null,
  };
}

/** End a response the application will never receive. */
function discard(response: HttpHandlerResponse): void {
  void response.body[Symbol.asyncIterator]().return?.();
}

/** The WHATWG `Response` for a handler response, or a refusal. */
export function toFetchResponse(response: HttpHandlerResponse): Response {
  const refuse = (message: string) => {
    discard(response);
    return new TypeError(`${message}; pass an IncomingHttpRequest to receive raw parts`);
  };
  if (typeof Response !== "function" || typeof Headers !== "function")
    throw refuse("WHATWG Response is unavailable on this host");
  if (response.status < 200) throw refuse(`HTTP status ${response.status} has no WHATWG Response`);
  const headers = new Headers();
  try {
    for (const [name, value] of response.fields) headers.append(name, byteString(value));
  } catch (error) {
    throw refuse(error instanceof Error ? error.message : String(error));
  }
  const iterator = response.body[Symbol.asyncIterator]();
  const empty = NULL_BODY.has(response.status);
  const body = empty
    ? null
    : new ReadableStream<Uint8Array>(
        {
          async pull(controller) {
            const next = await iterator.next();
            if (!next.done) return controller.enqueue(next.value);
            if ((await response.trailers)?.length)
              return controller.error(
                new TypeError("response trailers have no WHATWG representation"),
              );
            controller.close();
          },
          async cancel() {
            await iterator.return?.();
          },
        },
        { highWaterMark: 0 },
      );
  const result = new Response(body as ReadableStream<Uint8Array<ArrayBuffer>> | null, {
    status: response.status,
    headers,
  });
  for (const [name] of response.fields)
    if (!result.headers.has(name)) {
      void iterator.return?.();
      throw new TypeError(
        `the platform Response drops the ${name} field; pass an IncomingHttpRequest to receive raw parts`,
      );
    }
  if (empty) void drainEmpty(iterator);
  return result;
}

/**
 * A null-body status still completes its delivery: an empty body ends normally, while content
 * that has no place in such a response is refused to the handler as a disconnect.
 */
async function drainEmpty(iterator: AsyncIterator<Uint8Array>): Promise<void> {
  try {
    const next = await iterator.next();
    if (!next.done) await iterator.return?.();
  } catch {
    // The handler's failure ends its own delivery.
  }
}
