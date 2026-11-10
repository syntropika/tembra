/**
 * Synchronous value semantics of outgoing standard `wasi:http@0.3.0` in the browser: the
 * `fields`, `request-options`, `request` and `response` resources and request-target
 * validation. Nothing here performs I/O or uses host services; bodies and transport lifetime
 * live in `standard-http.ts`.
 *
 * Semantics follow pinned upstream `wasmtime-wasi-http` 51.0.0-rc.1 (`p3/host/types.rs`,
 * `field_map.rs`, `p3/request.rs`): raw byte field values, explicit duplicates, the
 * forbidden-field filter and the 128 KiB field budget, immutable headers/options views, and the
 * same request target defaults and errors. Target hosts follow the native host's
 * `Target::parse` rules.
 */
import type {
  ErrorCode,
  HeaderError,
  Method,
  RequestOptionsError,
  Result,
  Scheme,
} from "wasi:http/types@0.3.0";
import { type HttpField, HttpProviderError, type HttpTimeouts } from "./http-provider.ts";
import type {
  HostFuture,
  OutputStream,
  StandardResource,
  StandardServices,
  FutureReader,
} from "./standard-streams.ts";

/** Upstream `DEFAULT_FIELD_SIZE_LIMIT`. */
const FIELD_BYTES = 128 << 10;
/** Upstream accounts each name and value with its in-memory header size. */
const NAME_COST = 32;
const VALUE_COST = 40;
/** Upstream `DEFAULT_FORBIDDEN_HEADERS`. */
const FORBIDDEN = new Set([
  "connection",
  "keep-alive",
  "proxy-authenticate",
  "proxy-authorization",
  "proxy-connection",
  "transfer-encoding",
  "upgrade",
  "host",
  "http2-settings",
]);
const TOKEN = /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/;
const SCHEME = /^[A-Za-z][A-Za-z0-9+\-.]*$/;
const STANDARD_METHODS = [
  "get",
  "head",
  "post",
  "put",
  "delete",
  "connect",
  "options",
  "trace",
  "patch",
] as const;
const U64_MAX = (1n << 64n) - 1n;

export type Outcome = Result<void, ErrorCode>;
export type Trailers = Result<HttpFields | undefined, ErrorCode>;

export const ok = <T>(val: T): { tag: "ok"; val: T } => ({ tag: "ok", val });
export const err = <E>(val: E): { tag: "err"; val: E } => ({ tag: "err", val });
export const internal = (message: string): ErrorCode => ({ tag: "internal-error", val: message });
export const done: Outcome = ok(undefined);

/** ASCII digits of a validated `Content-Length` value. */
export const digits = (bytes: Uint8Array): string => String.fromCharCode(...bytes);

/** Reference counting for resources a handle, a call or a value may hold independently. */
export abstract class HttpResource implements StandardResource {
  borrows = 0;
  #users = 1;

  retain(): void {
    this.#users++;
  }

  release(): void {
    if (--this.#users === 0) this.close();
  }

  /** The last user went away. */
  protected close(): void {}
}

// ---------------------------------------------------------------------------------------------
// fields

type FieldFailure = HeaderError["tag"];

/** A field name in the canonical lowercase form upstream's `HeaderName` stores. */
function fieldName(name: string): string | undefined {
  return name.length < 0x10000 && TOKEN.test(name) ? name.toLowerCase() : undefined;
}

/** Upstream `HeaderValue` bytes: visible ASCII, obs-text and tab, never controls or DEL. */
function fieldValue(name: string, value: Uint8Array): Uint8Array | undefined {
  for (const byte of value) if ((byte < 0x20 && byte !== 0x09) || byte === 0x7f) return undefined;
  if (name !== "content-length") return value;
  // `Content-Length` is exactly `1*DIGIT` within u64, stored normalized.
  if (!value.length || value.some((byte) => byte < 0x30 || byte > 0x39)) return undefined;
  const length = BigInt(digits(value));
  if (length > U64_MAX) return undefined;
  return Uint8Array.from(length.toString(), (digit) => digit.charCodeAt(0));
}

/** `wasi:http/types#fields`: names in canonical lowercase, values as raw bytes in order. */
export class HttpFields extends HttpResource {
  readonly #entries: Map<string, readonly Uint8Array[]>;
  #size: number;
  readonly #mutable: boolean;

  private constructor(entries: Map<string, readonly Uint8Array[]>, size: number, mutable: boolean) {
    super();
    this.#entries = entries;
    this.#size = size;
    this.#mutable = mutable;
  }

  /** `constructor`: empty and mutable. */
  static create(): HttpFields {
    return new HttpFields(new Map(), 0, true);
  }

  /** `[static] from-list`: mutable; the first refused entry refuses the whole list. */
  static fromList(entries: [string, Uint8Array][]): Result<HttpFields, HeaderError> {
    const fields = HttpFields.create();
    for (const [name, value] of entries) {
      const appended = fields.append(name, value);
      if (appended.tag === "err") return appended;
    }
    return ok(fields);
  }

  /**
   * Immutable fields received from a provider. Forbidden fields are stripped before they can
   * reach the guest, as upstream does; a malformed name or value is the provider's error.
   */
  static received(pairs: readonly HttpField[]): HttpFields {
    const entries = new Map<string, Uint8Array[]>();
    let size = 0;
    for (const pair of pairs) {
      const [name, value] = Array.isArray(pair) ? pair : [];
      const key = typeof name === "string" ? fieldName(name) : undefined;
      const checked =
        key !== undefined && value instanceof Uint8Array ? fieldValue(key, value) : undefined;
      if (key === undefined || !checked)
        throw new HttpProviderError(internal(`the HTTP provider returned an invalid field`));
      if (FORBIDDEN.has(key)) continue;
      const values = entries.get(key);
      if (values) values.push(checked.slice());
      else {
        entries.set(key, [checked.slice()]);
        size += key.length + NAME_COST;
      }
      size += checked.length + VALUE_COST;
    }
    return new HttpFields(entries, size, false);
  }

  /** Whether any field is present. */
  get empty(): boolean {
    return this.#entries.size === 0;
  }

  /** An immutable snapshot, as upstream marks fields moved into a request or response. */
  frozen(): HttpFields {
    return new HttpFields(new Map(this.#entries), this.#size, false);
  }

  /** Every name/value pair in serialization order. */
  /** Every name/value pair in serialization order; values are copies the caller owns. */
  pairs(): [string, Uint8Array][] {
    const pairs: [string, Uint8Array][] = [];
    for (const [name, values] of this.#entries)
      for (const value of values) pairs.push([name, value.slice()]);
    return pairs;
  }

  #refuse(tag: FieldFailure): Result<never, HeaderError> {
    return err({ tag } as HeaderError);
  }

  /** Common name checks of every mutation, in upstream order. */
  #mutation(name: string): string | FieldFailure {
    const key = fieldName(name);
    if (key === undefined) return "invalid-syntax";
    if (FORBIDDEN.has(key)) return "forbidden";
    if (!this.#mutable) return "immutable";
    return key;
  }

  get(name: string): Uint8Array[] {
    const key = fieldName(name);
    return key === undefined ? [] : [...(this.#entries.get(key) ?? [])];
  }

  has(name: string): boolean {
    const key = fieldName(name);
    return key !== undefined && this.#entries.has(key);
  }

  set(name: string, values: Uint8Array[]): Result<void, HeaderError> {
    const key = this.#mutation(name);
    if (key === "invalid-syntax" || key === "forbidden" || key === "immutable")
      return this.#refuse(key);
    if (key === "content-length" && values.length > 1) return this.#refuse("invalid-syntax");
    const parsed: Uint8Array[] = [];
    for (const value of values) {
      const checked = fieldValue(key, value);
      if (!checked) return this.#refuse("invalid-syntax");
      parsed.push(checked.slice());
    }
    const previous = this.#entries.get(key);
    const removed = previous
      ? key.length + NAME_COST + previous.reduce((sum, v) => sum + v.length + VALUE_COST, 0)
      : 0;
    const added = parsed.length
      ? key.length + NAME_COST + parsed.reduce((sum, v) => sum + v.length + VALUE_COST, 0)
      : 0;
    if (parsed.length && this.#size - removed + added > FIELD_BYTES)
      return this.#refuse("size-exceeded");
    this.#entries.delete(key);
    if (parsed.length) this.#entries.set(key, parsed);
    this.#size += added - removed;
    return done as Result<void, HeaderError>;
  }

  delete(name: string): Result<void, HeaderError> {
    const removed = this.getAndDelete(name);
    return removed.tag === "err" ? removed : (done as Result<void, HeaderError>);
  }

  getAndDelete(name: string): Result<Uint8Array[], HeaderError> {
    const key = this.#mutation(name);
    if (key === "invalid-syntax" || key === "forbidden" || key === "immutable")
      return this.#refuse(key);
    const values = this.#entries.get(key);
    if (!values) return ok([]);
    this.#entries.delete(key);
    this.#size -=
      key.length + NAME_COST + values.reduce((sum, v) => sum + v.length + VALUE_COST, 0);
    return ok([...values]);
  }

  append(name: string, value: Uint8Array): Result<void, HeaderError> {
    const key = fieldName(name);
    if (key === undefined) return this.#refuse("invalid-syntax");
    if (FORBIDDEN.has(key)) return this.#refuse("forbidden");
    const checked = fieldValue(key, value);
    if (!checked) return this.#refuse("invalid-syntax");
    // Several `Content-Length` values would make message framing ambiguous.
    if (key === "content-length" && this.#entries.has(key)) return this.#refuse("invalid-syntax");
    if (!this.#mutable) return this.#refuse("immutable");
    const previous = this.#entries.get(key);
    const added = checked.length + VALUE_COST + (previous ? 0 : key.length + NAME_COST);
    if (this.#size + added > FIELD_BYTES) return this.#refuse("size-exceeded");
    this.#entries.set(key, [...(previous ?? []), checked.slice()]);
    this.#size += added;
    return done as Result<void, HeaderError>;
  }

  copyAll(): [string, Uint8Array][] {
    return this.pairs();
  }

  /** A deep, mutable copy. */
  clone(): HttpFields {
    return new HttpFields(new Map(this.#entries), this.#size, true);
  }
}

// ---------------------------------------------------------------------------------------------
// request-options

type OptionsResult = Result<void, RequestOptionsError>;

/** `wasi:http/types#request-options`; views obtained from a request are immutable. */
export class HttpRequestOptions extends HttpResource {
  #timeouts: HttpTimeouts;

  constructor(
    timeouts: HttpTimeouts = {},
    readonly mutable = true,
  ) {
    super();
    this.#timeouts = timeouts;
  }

  /** The current timeouts, as a request keeps them once the options moved into it. */
  get timeouts(): HttpTimeouts {
    return this.#timeouts;
  }

  #set(change: HttpTimeouts): OptionsResult {
    if (!this.mutable) return err({ tag: "immutable" } as RequestOptionsError);
    this.#timeouts = { ...this.#timeouts, ...change };
    return done as OptionsResult;
  }

  getConnectTimeout(): bigint | undefined {
    return this.#timeouts.connect;
  }

  /** Kept for the provider, which honors or refuses it at send. */
  setConnectTimeout(duration: bigint | undefined): OptionsResult {
    return this.#set({ connect: duration });
  }

  getFirstByteTimeout(): bigint | undefined {
    return this.#timeouts.firstByte;
  }

  setFirstByteTimeout(duration: bigint | undefined): OptionsResult {
    return this.#set({ firstByte: duration });
  }

  getBetweenBytesTimeout(): bigint | undefined {
    return this.#timeouts.betweenBytes;
  }

  setBetweenBytesTimeout(duration: bigint | undefined): OptionsResult {
    return this.#set({ betweenBytes: duration });
  }

  clone(): HttpRequestOptions {
    return new HttpRequestOptions(this.#timeouts, true);
  }
}

// ---------------------------------------------------------------------------------------------
// bodies (implemented in `standard-http.ts`)

/** The contents and trailers of one request or response, consumed or dropped once. */
export interface Body {
  /** `consume-body`: the contents stream and trailers future; `res` reports the consumer. */
  consume(host: StandardServices, res: FutureReader<Outcome>): [OutputStream, HostFuture];
  /** Dropped unconsumed. */
  close(): void;
}

// ---------------------------------------------------------------------------------------------
// request and response

/** State shared by requests and responses: immutable headers and a body that moves once. */
export abstract class Message extends HttpResource {
  #body: Body | undefined;

  constructor(
    readonly headers: HttpFields,
    body: Body,
  ) {
    super();
    this.#body = body;
  }

  /** Move the body out; afterwards the message holds only its metadata. */
  take(): Body {
    const body = this.#body;
    if (!body) throw new Error("HTTP body was already consumed");
    this.#body = undefined;
    return body;
  }

  /** An immutable view; each call issues a new resource. */
  getHeaders(): HttpFields {
    return this.headers.frozen();
  }

  protected override close(): void {
    this.#body?.close();
    this.#body = undefined;
  }
}

/** Upstream `parse_authority`: an `http::uri::Authority` without user info and a valid port. */
function authority(text: string): boolean {
  if (!text || text.includes("@") || !/^[A-Za-z0-9\-._~!$&'()*+,;=:%[\]]+$/.test(text))
    return false;
  let host = text;
  let port: string | undefined;
  if (text.startsWith("[")) {
    const close = text.indexOf("]");
    if (close < 0 || text.indexOf("[", 1) >= 0 || text.indexOf("]", close + 1) >= 0) return false;
    const rest = text.slice(close + 1);
    if (rest && !rest.startsWith(":")) return false;
    host = text.slice(0, close + 1);
    port = rest ? rest.slice(1) : undefined;
  } else {
    if (/[[\]]/.test(text)) return false;
    const colon = text.indexOf(":");
    if (colon >= 0) {
      if (text.indexOf(":", colon + 1) >= 0) return false;
      host = text.slice(0, colon);
      port = text.slice(colon + 1);
    }
  }
  if (port !== undefined && !(/^[0-9]+$/.test(port) && Number(port) <= 0xffff)) return false;
  return host.length > 0 || port !== undefined;
}

/** Upstream `parse_path_with_query`: an origin-form path and query, fragment discarded. */
function pathWithQuery(text: string): string | undefined {
  if (!text || text.length > 0xfffe || !(text.startsWith("/") || text.startsWith("?")))
    return undefined;
  let query = false;
  for (let at = 0; at < text.length; at++) {
    const code = text.charCodeAt(at);
    if (code === 0x23) return text.slice(0, at);
    if (code === 0x3f && !query) {
      query = true;
      continue;
    }
    if (code >= 0x80) continue;
    const valid = query
      ? code === 0x21 ||
        (code >= 0x24 && code <= 0x3b) ||
        code === 0x3d ||
        (code >= 0x3f && code <= 0x7e)
      : code === 0x21 ||
        (code >= 0x24 && code <= 0x3b) ||
        code === 0x3d ||
        (code >= 0x40 && code <= 0x5f) ||
        (code >= 0x61 && code <= 0x7a) ||
        code === 0x7c ||
        code === 0x7e ||
        code === 0x22 ||
        code === 0x7b ||
        code === 0x7d;
    if (!valid) return undefined;
  }
  return text;
}

/** A checked request target. */
export interface Target {
  /** Canonical origin plus the WHATWG-normalized path, for grants and URL-based transports. */
  readonly url: URL;
  /** The validated raw origin-form path and query, as the guest set it. */
  readonly pathWithQuery: string;
}

/** A dotted quad of decimal octets without leading zeros, as Rust's `Ipv4Addr` parses it. */
const IPV4 = /^(?:(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)\.){3}(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)$/;
const LABEL = /^(?!-)[a-z0-9-]{1,63}(?<!-)$/;

/**
 * The native host's `Target::parse` host rules, applied to an authority that already passed
 * `authority`: an IP literal (strict dotted quad, or any valid bracketed IPv6 spelling in its
 * canonical form) or a lowercase LDH DNS name whose last label is not numeric (so URL parsers
 * cannot read it as an IPv4 shorthand), plus a u16 port (leading zeros allowed, 0 refused).
 */
export function canonicalHost(
  authority: string,
  secure: boolean,
): { name: string; port: number } | undefined {
  const close = authority.startsWith("[") ? authority.indexOf("]") + 1 : 0;
  const colon = authority.indexOf(":", close);
  const host = colon < 0 ? authority : authority.slice(0, colon);
  const port = colon < 0 ? (secure ? 443 : 80) : Number(authority.slice(colon + 1));
  if (!Number.isInteger(port) || port < 1 || port > 0xffff) return undefined;
  if (close) {
    try {
      // URL validates the literal and prints its canonical form.
      return { name: new URL(`http://${host}/`).hostname, port };
    } catch {
      return undefined;
    }
  }
  const name = host.toLowerCase();
  if (IPV4.test(name)) return { name, port };
  const labels = name.replace(/\.$/, "").split(".");
  const last = labels.at(-1)!;
  if (
    name.length > 253 ||
    !labels.every((label) => LABEL.test(label)) ||
    /^\d+$/.test(last) ||
    last.startsWith("0x")
  )
    return undefined;
  return { name, port };
}

/** `wasi:http/types#request` built by the guest. */
export class HttpRequest extends Message {
  #method: Method = { tag: "get" };
  #scheme: Scheme | undefined;
  #authority: string | undefined;
  #path: string | undefined;

  constructor(
    headers: HttpFields,
    readonly options: HttpTimeouts | undefined,
    body: Body,
  ) {
    super(headers, body);
  }

  getMethod(): Method {
    return this.#method;
  }

  /** Standard names normalize to their cases, as upstream's `http::Method` does. */
  setMethod(method: Method): Result<void, void> {
    if (method.tag === "other") {
      if (!TOKEN.test(method.val)) return err(undefined);
      const standard = STANDARD_METHODS.find((name) => name.toUpperCase() === method.val);
      this.#method = standard ? ({ tag: standard } as Method) : { tag: "other", val: method.val };
    } else if (STANDARD_METHODS.includes(method.tag)) this.#method = { tag: method.tag } as Method;
    else return err(undefined);
    return ok(undefined);
  }

  getPathWithQuery(): string | undefined {
    return this.#path;
  }

  setPathWithQuery(path: string | undefined): Result<void, void> {
    if (path === undefined) {
      this.#path = undefined;
      return ok(undefined);
    }
    const parsed = pathWithQuery(path);
    if (parsed === undefined) return err(undefined);
    this.#path = parsed;
    return ok(undefined);
  }

  getScheme(): Scheme | undefined {
    return this.#scheme;
  }

  setScheme(scheme: Scheme | undefined): Result<void, void> {
    if (scheme === undefined || scheme.tag === "HTTP" || scheme.tag === "HTTPS") {
      this.#scheme = scheme && ({ tag: scheme.tag } as Scheme);
      return ok(undefined);
    }
    if (scheme.tag !== "other" || scheme.val.length > 64 || !SCHEME.test(scheme.val))
      return err(undefined);
    const lower = scheme.val.toLowerCase();
    this.#scheme =
      lower === "http"
        ? { tag: "HTTP" }
        : lower === "https"
          ? { tag: "HTTPS" }
          : { tag: "other", val: scheme.val };
    return ok(undefined);
  }

  getAuthority(): string | undefined {
    return this.#authority;
  }

  setAuthority(value: string | undefined): Result<void, void> {
    if (value !== undefined && !authority(value)) return err(undefined);
    this.#authority = value;
    return ok(undefined);
  }

  getOptions(): HttpRequestOptions | undefined {
    return this.options && new HttpRequestOptions(this.options, false);
  }

  /** The method as sent on the wire: a standard name in uppercase, or the extension token. */
  get methodName(): string {
    return this.#method.tag === "other" ? this.#method.val : this.#method.tag.toUpperCase();
  }

  /**
   * The checked target, with upstream's defaults and errors: no scheme means HTTPS, only
   * HTTP(S) is supported, and authority and path are required. The host must pass the native
   * host's `Target::parse` rules (see `canonicalHost`), otherwise the request is denied. `url`
   * names the canonical origin the grant is checked against; `pathWithQuery` stays exactly as
   * the guest validated it, independent of URL normalization.
   */
  target(): Result<Target, ErrorCode> {
    const scheme = this.#scheme ?? { tag: "HTTPS" };
    if (scheme.tag === "other") return err({ tag: "HTTP-protocol-error" });
    if (this.#authority === undefined || this.#path === undefined)
      return err({ tag: "HTTP-request-URI-invalid" });
    const secure = scheme.tag === "HTTPS";
    const host = canonicalHost(this.#authority, secure);
    if (!host) return err({ tag: "HTTP-request-denied" });
    let url: URL;
    try {
      url = new URL(`${secure ? "https" : "http"}://${host.name}:${host.port}${this.#path}`);
    } catch {
      return err({ tag: "HTTP-request-URI-invalid" });
    }
    // The canonical host is a fixed point of URL parsing; anything else is refused.
    if (url.hostname !== host.name) return err({ tag: "HTTP-request-denied" });
    return ok({ url, pathWithQuery: this.#path });
  }
}

/** `wasi:http/types#response`: received from a provider, or built by the guest. */
export class HttpResponse extends Message {
  #status: number;

  constructor(headers: HttpFields, body: Body, status = 200) {
    super(headers, body);
    this.#status = status;
  }

  getStatusCode(): number {
    return this.#status;
  }

  setStatusCode(status: number): Result<void, void> {
    if (status < 100 || status > 599) return err(undefined);
    this.#status = status;
    return ok(undefined);
  }
}
