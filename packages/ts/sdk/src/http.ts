import { Reader, decodeText, encodeText } from "./http/bytes.ts";
import { events, type ServerSentEvent } from "./http/events.ts";
import type { TransportStream } from "./guest.ts";

export type { ServerSentEvent } from "./http/events.ts";
export type { TransportStream } from "./guest.ts";

export interface HttpRequest {
  method?: string;
  headers?: Readonly<Record<string, string>>;
  body?: string | Uint8Array;
}

/** Consume the body once. Iteration and SSE apply backpressure and close on an early return. */
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

const TOKEN = /^[!#$%&'*+.^_`|~0-9A-Za-z-]+$/;
const BODY_LIMIT = 1 << 20;

/** HTTP framing, body decoding and SSE run in the guest; the host only transports bytes. */
export async function fetch(url: string, options: HttpRequest = {}): Promise<HttpResponse> {
  // oxlint-disable-next-line no-control-regex
  const match = /^http:\/\/(\[[0-9a-fA-F:]+\]|[0-9.]+)(?::([0-9]+))?([/?][^\x00-\x20#]*)?$/.exec(
    url,
  );
  if (!match) throw new TypeError("HTTP requires an http: URL with a literal IP and no fragment");
  const host = match[1]!;
  const ip = host.startsWith("[") ? host.slice(1, -1) : host;
  const port = Number(match[2] ?? 80);
  if (!Number.isInteger(port) || port < 1 || port > 65535) throw new TypeError("Invalid HTTP port");
  const method = (options.method ?? "GET").toUpperCase();
  if (!TOKEN.test(method)) throw new TypeError("Invalid HTTP method");
  if ((method === "GET" || method === "HEAD") && options.body !== undefined)
    throw new TypeError(`${method} cannot send a body`);
  const body = typeof options.body === "string" ? encodeText(options.body) : options.body;
  if (body && !(body instanceof Uint8Array))
    throw new TypeError("HTTP body requires bytes or text");
  const target = match[3]?.startsWith("?") ? `/${match[3]}` : (match[3] ?? "/");
  const path = target.replace(/[^\x21-\x7e]/gu, (character) =>
    [...encodeText(character)]
      .map((byte) => `%${byte.toString(16).padStart(2, "0").toUpperCase()}`)
      .join(""),
  );
  let request = `${method} ${path} HTTP/1.1\r\nHost: ${host}:${port}\r\nConnection: close\r\nAccept-Encoding: identity\r\n`;
  for (const [name, value] of Object.entries(options.headers ?? {})) {
    if (!TOKEN.test(name) || /[^\x20-\x7e\t]/.test(value))
      throw new TypeError("Invalid HTTP header");
    if (
      ["host", "connection", "content-length", "transfer-encoding", "accept-encoding"].includes(
        name.toLowerCase(),
      )
    )
      throw new TypeError(`HTTP manages the ${name} header`);
    request += `${name}: ${value}\r\n`;
  }
  if (body !== undefined || ["POST", "PUT", "PATCH"].includes(method))
    request += `Content-Length: ${body?.length ?? 0}\r\n`;
  request += "\r\n";
  if (request.length > 65536) throw new TypeError("HTTP headers exceed the limit");
  const transport = (
    globalThis as unknown as {
      tembra?: { transport?: { connect(ip: string, port: number): Promise<TransportStream> } };
    }
  ).tembra?.transport;
  if (!transport) throw new Error("This Orb has no Tembra transport implementation");
  const stream = await transport.connect(ip, port);
  let closing: Promise<void> | undefined;
  const close = () => (closing ??= stream.close());
  try {
    for (const bytes of [encodeText(request), body]) {
      if (!bytes) continue;
      for (let offset = 0; offset < bytes.length; offset += 16384)
        await stream.write(bytes.subarray(offset, offset + 16384));
    }
    const reader = new Reader(stream);
    let status = 0;
    let headers = new Map<string, string>();
    for (let interim = 0; ; interim++) {
      if (interim > 8) throw new Error("Too many interim HTTP responses");
      const parsed = /^HTTP\/1\.[01] ([1-5][0-9]{2})(?: [\x20-\x7e\x80-\xff\t]*)?$/.exec(
        await reader.line(),
      );
      if (!parsed) throw new Error("Invalid HTTP status line");
      status = Number(parsed[1]);
      headers = await readHeaders(reader);
      if (status === 101) throw new Error("HTTP protocol upgrades require another transport");
      if (status >= 200) break;
    }
    const encoding = headers.get("content-encoding");
    if (encoding && encoding.toLowerCase() !== "identity")
      throw new Error("Unsupported HTTP content encoding");
    const transfer = headers.get("transfer-encoding")?.toLowerCase();
    const contentLength = headers.get("content-length");
    if (transfer && (transfer !== "chunked" || contentLength !== undefined))
      throw new Error("Unsupported or ambiguous HTTP body framing");
    const length = contentLength === undefined ? undefined : Number(contentLength);
    if (
      contentLength !== undefined &&
      (!/^[0-9]+$/.test(contentLength) || !Number.isSafeInteger(length))
    )
      throw new Error("Invalid HTTP Content-Length");
    let consumed = false;
    const empty = method === "HEAD" || status === 204 || status === 304 || length === 0;
    if (empty) await close();
    const responseBody: AsyncIterable<Uint8Array> = {
      async *[Symbol.asyncIterator]() {
        if (consumed) throw new Error("HTTP body has already been consumed");
        consumed = true;
        try {
          if (empty) return;
          if (transfer === "chunked") {
            for (;;) {
              const parsed = /^([0-9a-fA-F]+)(?:;[\x20-\x7e]*)?$/.exec(await reader.line());
              const size = parsed ? Number.parseInt(parsed[1]!, 16) : NaN;
              if (!Number.isSafeInteger(size) || size < 0)
                throw new Error("Invalid HTTP chunk size");
              if (!size) {
                const trailers = await readHeaders(reader);
                if (trailers.has("content-length") || trailers.has("transfer-encoding"))
                  throw new Error("HTTP trailers cannot change body framing");
                return;
              }
              yield* sizedBody(reader, size);
              if ((await reader.line(2)) !== "") throw new Error("Invalid HTTP chunk delimiter");
            }
          } else if (length !== undefined) {
            yield* sizedBody(reader, length);
          } else {
            for (;;) {
              const bytes = await reader.read();
              if (bytes === null) return;
              yield bytes;
            }
          }
        } finally {
          try {
            await close();
          } catch {
            /* Cleanup cannot replace a framing or cancellation failure. */
          }
        }
      },
    };
    return Object.freeze({
      status,
      ok: status >= 200 && status < 300,
      headers,
      body: responseBody,
      text: () => decodeText(responseBody, BODY_LIMIT),
      async json<T>() {
        return JSON.parse(await decodeText(responseBody, BODY_LIMIT)) as T;
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
          try {
            await close();
          } catch {
            /* The body outcome retains precedence over cleanup. */
          }
        }
      },
      close,
    });
  } catch (error) {
    try {
      await close();
    } catch {
      /* Preserve the framing or transport failure. */
    }
    throw error;
  }
}

/** Bound framing metadata independently of an unbounded streamed response body. */
async function readHeaders(reader: Reader): Promise<Map<string, string>> {
  const headers = new Map<string, string>();
  let bytes = 0;
  for (;;) {
    const line = await reader.line();
    bytes += line.length + 2;
    if (bytes > 65536) throw new Error("HTTP headers exceed the limit");
    if (!line) return headers;
    const field = /^([!#$%&'*+.^_`|~0-9A-Za-z-]+):[ \t]*([\x20-\x7e\x80-\xff\t]*)$/.exec(line);
    if (!field) throw new Error("Invalid HTTP header");
    const name = field[1]!.toLowerCase();
    const value = field[2]!.replace(/[ \t]+$/u, "");
    if (headers.has(name) && (name === "content-length" || name === "transfer-encoding"))
      throw new Error("Duplicate HTTP framing header");
    headers.set(name, headers.has(name) ? `${headers.get(name)}, ${value}` : value);
  }
}

async function* sizedBody(reader: Reader, size: number): AsyncIterable<Uint8Array> {
  while (size) {
    const bytes = await reader.read(size);
    if (bytes === null) throw new Error("Truncated HTTP body");
    size -= bytes.length;
    yield bytes;
  }
}
