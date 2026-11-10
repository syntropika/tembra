import { decodeText, encodeText } from "./http/bytes.ts";
import { events, type ServerSentEvent } from "./http/events.ts";
import type { HttpStream } from "./guest.ts";

export type { ServerSentEvent } from "./http/events.ts";

export interface HttpRequest {
  method?: string;
  headers?: Readonly<Record<string, string>>;
  body?: string | Uint8Array;
}

/** Consume the body once, streaming bytes or parsed events from the host's WASI HTTP response. */
export interface HttpResponse {
  readonly status: number;
  readonly ok: boolean;
  readonly headers: ReadonlyMap<string, string>;
  readonly body: AsyncIterable<Uint8Array>;
  text(): Promise<string>;
  json<T = unknown>(): Promise<T>;
  events(): AsyncIterable<ServerSentEvent>;
  close(): Promise<void>;
}

/** HTTP and TLS execute through the host; this facade only adapts data and owns body consumption. */
export async function fetch(url: string, options: HttpRequest = {}): Promise<HttpResponse> {
  const http = typeof tembra === "undefined" ? undefined : tembra.http;
  if (!http) throw new Error("This Orb has no WASI HTTP provider");
  const body = typeof options.body === "string" ? encodeText(options.body) : options.body;
  if (body !== undefined && !(body instanceof Uint8Array))
    throw new TypeError("HTTP body requires bytes or text");
  let method = options.method ?? "GET";
  if (["DELETE", "GET", "HEAD", "OPTIONS", "POST", "PUT"].includes(method.toUpperCase()))
    method = method.toUpperCase();
  if (["CONNECT", "TRACE", "TRACK"].includes(method.toUpperCase()))
    throw new TypeError(`HTTP method ${method} is forbidden`);
  if ((method === "GET" || method === "HEAD") && body !== undefined)
    throw new TypeError(`${method} requests cannot have a body`);
  const stream: HttpStream = await http.send(
    url,
    method,
    Object.entries(options.headers ?? {}),
    body ?? new Uint8Array(),
  );
  const headers = new Map(stream.headers.map(([name, value]) => [name.toLowerCase(), value]));
  let closing: Promise<void> | undefined;
  const close = () => (closing ??= stream.close());
  const cleanup = async () => {
    try {
      await close();
    } catch {
      /* Cleanup cannot replace a body or cancellation failure. */
    }
  };
  let consumed = false;
  const responseBody: AsyncIterable<Uint8Array> = {
    async *[Symbol.asyncIterator]() {
      if (consumed) throw new Error("HTTP body has already been consumed");
      consumed = true;
      try {
        for (;;) {
          const bytes = await stream.read();
          if (bytes === null) return;
          yield bytes;
        }
      } finally {
        await cleanup();
      }
    },
  };
  return Object.freeze({
    status: stream.status,
    ok: stream.status >= 200 && stream.status < 300,
    headers,
    body: responseBody,
    text: () => decodeText(responseBody, 1 << 20),
    async json<T>() {
      return JSON.parse(await decodeText(responseBody, 1 << 20)) as T;
    },
    events: async function* () {
      try {
        if (
          headers.get("content-type")?.split(";", 1)[0]?.trim().toLowerCase() !==
          "text/event-stream"
        )
          throw new Error("SSE requires text/event-stream");
        yield* events(responseBody);
      } finally {
        await cleanup();
      }
    },
    close,
  });
}
