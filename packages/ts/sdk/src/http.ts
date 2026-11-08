/** A bounded UTF-8 HTTP/1 response from the guest-side GET helper. */
export interface HttpResponse {
  readonly status: number;
  readonly ok: boolean;
  readonly headers: ReadonlyMap<string, string>;
  text(): Promise<string>;
  json<T = unknown>(): Promise<T>;
}

interface Transport {
  exchange(ip: string, port: number, request: string): Promise<string>;
}

/** Fetch an HTTP URL using guest-side framing and a separately granted raw transport. */
export async function fetch(url: string): Promise<HttpResponse> {
  // Reject HTTP request-line control characters before passing text to the transport.
  // oxlint-disable-next-line no-control-regex
  const match = /^http:\/\/(\[[0-9a-fA-F:]+\]|[0-9.]+)(?::([0-9]+))?(\/[^\x00-\x20#]*)?$/.exec(url);
  if (!match)
    throw new TypeError("This HTTP helper requires an http: URL with a literal IP and no fragment");
  const host = match[1]!;
  const ip = host.startsWith("[") ? host.slice(1, -1) : host;
  const port = Number(match[2] ?? 80);
  if (!Number.isInteger(port) || port < 1 || port > 65535) throw new TypeError("Invalid HTTP port");
  const transport = (globalThis as unknown as { tembra?: { transport?: Transport } }).tembra
    ?.transport;
  if (!transport) throw new Error("This Orb has no Tembra transport implementation");
  const raw = await transport.exchange(
    ip,
    port,
    `GET ${match[3] ?? "/"} HTTP/1.1\r\nHost: ${host}:${port}\r\nConnection: close\r\nAccept: application/json\r\n\r\n`,
  );
  const boundary = raw.indexOf("\r\n\r\n");
  if (boundary < 0 || boundary > 65536) throw new Error("Invalid or oversized HTTP headers");
  const lines = raw.slice(0, boundary).split("\r\n");
  const status = /^HTTP\/1\.[01] ([0-9]{3})(?: .*)?$/.exec(lines.shift() ?? "");
  if (!status) throw new Error("Invalid HTTP status line");
  const headers = new Map<string, string>();
  for (const line of lines) {
    const field = /^([!#$%&'*+.^_`|~0-9A-Za-z-]+):[ \t]*([^\r\n]*)$/.exec(line);
    if (!field) throw new Error("Invalid HTTP header");
    const key = field[1]!.toLowerCase();
    if (headers.has(key))
      throw new Error("Duplicate HTTP headers are outside this helper's profile");
    headers.set(key, field[2]!.trim());
  }
  if (headers.has("transfer-encoding") || headers.has("content-encoding"))
    throw new Error("Encoded and streamed bodies require a different HTTP client profile");
  const body = raw.slice(boundary + 4);
  let bytes = 0;
  for (const character of body) {
    const point = character.codePointAt(0)!;
    bytes += point <= 0x7f ? 1 : point <= 0x7ff ? 2 : point <= 0xffff ? 3 : 4;
  }
  const length = headers.get("content-length");
  if (length !== undefined && (!/^[0-9]+$/.test(length) || Number(length) !== bytes))
    throw new Error("HTTP body length differs from Content-Length");
  const code = Number(status[1]);
  return Object.freeze({
    status: code,
    ok: code >= 200 && code < 300,
    headers,
    async text() {
      return body;
    },
    async json<T>() {
      return JSON.parse(body) as T;
    },
  });
}
