// Raw-packet guest of the browser StandardHost for the focused `wasi:http` tests. It speaks the
// exact v7 wire the generated guest bindings use (CALL, STREAM_*, FUTURE_*, DROP) and names
// handles by raw number, so hostile and lifetime cases stay explicit. Wire types are read from
// the generated metadata, never baked.
import { expect } from "bun:test";
import { BrowserNetwork } from "../src/interpreter/browser-network.ts";
import type { HttpProvider } from "../src/interpreter/http-provider.ts";
import { METHODS, RESOURCES, TYPES } from "../src/interpreter/standard.ts";
import {
  CALL,
  DROP,
  FUTURE_NEW,
  FUTURE_READ,
  FUTURE_WRITE,
  type HandleCodec,
  Reader,
  STREAM_NEW,
  STREAM_READ,
  STREAM_WRITE,
  Writer,
  decode,
  encode,
} from "../src/interpreter/standard-codec.ts";
import { StandardHost } from "../src/interpreter/standard-host.ts";

export const HTTP = "wasi:http/types@0.3.0";
export const CLIENT = "wasi:http/client@0.3.0";
export const ORIGIN = "https://api.example.test";
export const encoder = new TextEncoder();
export const decoder = new TextDecoder();
export const tick = () => new Promise((resolve) => setTimeout(resolve, 0));
/** A hostile guest names handles by raw number. */
export const raw: HandleCodec = {
  handle: (_kind, _detail, value) => value as number,
  value: (_kind, _detail, handle) => handle,
};

export function deferred<T = void>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((accept) => {
    resolve = accept;
  });
  return { promise, resolve };
}

/** Generated method index by resource (`undefined` for `client.send`) and name. */
export function method(resource: string | undefined, name: string): number {
  const found = METHODS.findIndex(
    (m) =>
      m.name === name &&
      (resource === undefined
        ? m.interface === CLIENT
        : m.resource !== undefined && RESOURCES[m.resource] === `${HTTP}#${resource}`),
  );
  if (found < 0) throw new Error(`${resource}.${name} is not in the generated table`);
  return found;
}

// Wire types read from the generated metadata, never baked indices.
export const TRAILERS = METHODS[method("request", "new")]!.params[2]!;
export const RES = METHODS[method("request", "consumeBody")]!.params[1]!;
export const BODY = TYPES[METHODS[method("request", "consumeBody")]!.result!]![1] as number[];
export const OUT_TRAILERS = TYPES[BODY[1]!]![1] as number;
export const RESULT = TYPES[
  (TYPES[METHODS[method("request", "new")]!.result!]![1] as number[])[1]!
]![1] as number;

export function decodeAs(type: number | undefined, bytes: Uint8Array): any {
  if (type === undefined) return undefined;
  const reader = new Reader(bytes);
  const value = decode(reader, type, raw);
  reader.end();
  return value;
}

/** A raw-packet guest of one StandardHost. */
export class Guest {
  readonly #payloads = new Map<number, number>();

  constructor(readonly host: StandardHost) {}

  #packet(operation: number, build: (writer: Writer) => void): Uint8Array {
    const writer = new Writer();
    writer.u8(0);
    writer.u8(operation);
    build(writer);
    return writer.finish();
  }

  packet(resource: string | undefined, name: string, args: unknown[]): Uint8Array {
    const index = method(resource, name);
    return this.#packet(CALL, (writer) => {
      writer.u32(index);
      METHODS[index]!.params.forEach((type, at) => encode(writer, type, args[at], raw));
    });
  }

  sync(resource: string, name: string, ...args: unknown[]): any {
    const bytes = this.host.sync(this.packet(resource, name, args));
    return decodeAs(METHODS[method(resource, name)]!.result, bytes);
  }

  async send(request: number, signal: AbortSignal): Promise<any> {
    const delivery = await this.host.async(this.packet(undefined, "send", [request]), signal);
    return decodeAs(METHODS[method(undefined, "send")]!.result, delivery.bytes);
  }

  fields(entries: [string, string | number[]][]): number {
    const created = this.sync(
      "fields",
      "fromList",
      entries.map(([name, value]) => [
        name,
        typeof value === "string" ? encoder.encode(value) : Uint8Array.from(value),
      ]),
    );
    expect(created.tag).toBe("ok");
    return created.val;
  }

  streamNew(): number {
    const type = TYPES.findIndex(([kind, item]) => kind === "stream" && item === "u8");
    const reply = this.host.sync(this.#packet(STREAM_NEW, (writer) => writer.u32(type)));
    return new Reader(reply).handle();
  }

  futureNew(type: number): number {
    const reply = this.host.sync(this.#packet(FUTURE_NEW, (writer) => writer.u32(type)));
    const handle = new Reader(reply).handle();
    this.#payloads.set(handle, TYPES[type]![1] as number);
    return handle;
  }

  futureWritePacket(handle: number, value: unknown, trailing = false): Uint8Array {
    return this.#packet(FUTURE_WRITE, (writer) => {
      writer.handle(handle);
      encode(writer, this.#payloads.get(handle)!, value, raw);
      if (trailing) writer.u8(0);
    });
  }

  futureWrite(handle: number, value: unknown): void {
    expect(this.host.sync(this.futureWritePacket(handle, value)).length).toBe(0);
  }

  async streamWrite(handle: number, bytes: Uint8Array, signal?: AbortSignal): Promise<void> {
    for (let at = 0; at < bytes.length; at += 65536) {
      const chunk = bytes.subarray(at, at + 65536);
      await this.host.async(
        this.#packet(STREAM_WRITE, (writer) => {
          writer.handle(handle);
          writer.u32(chunk.length);
          writer.raw(chunk);
        }),
        signal,
      );
    }
  }

  /** One read; `undefined` is end of stream. */
  async streamRead(handle: number, signal?: AbortSignal): Promise<Uint8Array | undefined> {
    const delivery = await this.host.async(
      this.#packet(STREAM_READ, (writer) => {
        writer.handle(handle);
        writer.u32(65536);
      }),
      signal,
    );
    const reader = new Reader(delivery.bytes);
    return reader.u8() === 1 ? undefined : reader.bytes();
  }

  async futureRead(handle: number, payload: number, signal?: AbortSignal): Promise<any> {
    const delivery = await this.host.async(
      this.#packet(FUTURE_READ, (writer) => writer.handle(handle)),
      signal,
    );
    return decodeAs(payload, delivery.bytes);
  }

  drop(handle: number): void {
    this.host.sync(this.#packet(DROP, (writer) => writer.handle(handle)));
  }

  /** A request to `url` with an empty, already completed upload unless told otherwise. */
  request(
    path = "/events",
    options: {
      headers?: [string, string | number[]][];
      contents?: number;
      trailers?: number;
      method?: unknown;
      options?: number;
      authority?: string;
    } = {},
  ): { request: number; result: number; trailers: number } {
    const trailers = options.trailers ?? this.futureNew(TRAILERS);
    const [request, result] = this.sync(
      "request",
      "new",
      this.fields(options.headers ?? []),
      options.contents,
      trailers,
      options.options,
    );
    if (options.trailers === undefined && options.contents === undefined)
      this.futureWrite(trailers, { tag: "ok" });
    if (options.method)
      expect(this.sync("request", "setMethod", request, options.method).tag).toBe("ok");
    expect(this.sync("request", "setScheme", request, { tag: "HTTPS" }).tag).toBe("ok");
    expect(
      this.sync("request", "setAuthority", request, options.authority ?? "api.example.test").tag,
    ).toBe("ok");
    expect(this.sync("request", "setPathWithQuery", request, path).tag).toBe("ok");
    return { request, result, trailers };
  }
}

export function standard(provider?: HttpProvider, origins = [ORIGIN]) {
  const network = new BrowserNetwork(
    { http: [{ provider: "mock", origins }] },
    provider ? { http: { mock: provider } } : {},
  );
  const host = new StandardHost(new Set([HTTP, CLIENT]), [], 8 << 20, { http: network });
  return { host, guest: new Guest(host) };
}

/** Fields entries as `[name, text]` pairs. */
export function text(pairs: [string, Uint8Array][]): [string, string][] {
  return pairs.map(([name, value]) => [name, decoder.decode(value)]);
}

export async function retire(host: StandardHost): Promise<void> {
  host.endCallback();
  await host.settled();
  await host.dispose();
  expect(host.size).toBe(0);
  expect(host.pending).toBe(0);
  expect(host.inputBytes).toBe(0);
}
