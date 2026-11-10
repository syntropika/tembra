// Generic future and ownership mechanics of the browser StandardHost (wire v7): FUTURE_NEW and
// FUTURE_WRITE transactions, writer abandonment, journaled handle-bearing FUTURE_READ values and
// owned-parameter transfer. The HTTP futures are the only generated writable types, so they
// carry the cases; nothing here depends on Fetch.
import { afterAll, beforeAll, expect, test } from "bun:test";
import { METHODS, RESOURCES, TYPES, WRITABLE_FUTURES } from "../src/interpreter/standard.ts";
import {
  CALL,
  DROP,
  FUTURE_NEW,
  FUTURE_READ,
  FUTURE_WRITE,
  type HandleCodec,
  Reader,
  STREAM_NEW,
  Writer,
  decode,
  encode,
} from "../src/interpreter/standard-codec.ts";
import { StandardHost } from "../src/interpreter/standard-host.ts";
import { HttpFields } from "../src/interpreter/standard-http-values.ts";

const HTTP = "wasi:http/types@0.3.0";
const CLIENT = "wasi:http/client@0.3.0";
const encoder = new TextEncoder();
const tick = () => new Promise((resolve) => setTimeout(resolve, 0));
const raw: HandleCodec = {
  handle: (_kind, _detail, value) => value as number,
  value: (_kind, _detail, handle) => handle,
};
const ABANDONED = { tag: "err", val: { tag: "internal-error", val: "callback ended" } };

function method(resource: string | undefined, name: string): number {
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

const TRAILERS = METHODS[method("request", "new")]!.params[2]!;
const RES = METHODS[method("request", "consumeBody")]!.params[1]!;
const BODY = TYPES[METHODS[method("request", "consumeBody")]!.result!]![1] as number[];
const OUT_TRAILERS = TYPES[BODY[1]!]![1] as number;

/** Count releases of host fields, which own no physical resource and are otherwise invisible. */
let released = 0;
const resource = Object.getPrototypeOf(HttpFields.prototype) as { release(): void };
const release = resource.release;
beforeAll(() => {
  resource.release = function (this: unknown) {
    if (this instanceof HttpFields) released++;
    release.call(this);
  };
});
afterAll(() => {
  resource.release = release;
});

function packet(operation: number, build: (writer: Writer) => void): Uint8Array {
  const writer = new Writer();
  writer.u8(0);
  writer.u8(operation);
  build(writer);
  return writer.finish();
}

function setup() {
  const host = new StandardHost(new Set([HTTP, CLIENT]), [], 8 << 20, {
    http: { provider: () => undefined },
  });
  const callPacket = (resource: string | undefined, name: string, args: unknown[]) => {
    const index = method(resource, name);
    return packet(CALL, (writer) => {
      writer.u32(index);
      METHODS[index]!.params.forEach((type, at) => encode(writer, type, args[at], raw));
    });
  };
  const sync = (resource: string, name: string, ...args: unknown[]): any => {
    const reader = new Reader(host.sync(callPacket(resource, name, args)));
    const type = METHODS[method(resource, name)]!.result;
    const value = type === undefined ? undefined : decode(reader, type, raw);
    reader.end();
    return value;
  };
  const fields = (name = "x-a") => {
    const created = sync("fields", "fromList", [[name, encoder.encode("1")]]);
    return created.val as number;
  };
  const futureNew = (type: number) =>
    new Reader(host.sync(packet(FUTURE_NEW, (writer) => writer.u32(type)))).handle();
  const write = (handle: number, payload: number, value: unknown, trailing = false) =>
    host.sync(
      packet(FUTURE_WRITE, (writer) => {
        writer.handle(handle);
        encode(writer, payload, value, raw);
        if (trailing) writer.u8(0);
      }),
    );
  const read = async (handle: number, payload: number, signal?: AbortSignal) => {
    const delivery = await host.async(
      packet(FUTURE_READ, (writer) => writer.handle(handle)),
      signal,
    );
    const reader = new Reader(delivery.bytes);
    const value = decode(reader, payload, raw);
    reader.end();
    return { value, issued: delivery.issued };
  };
  const drop = (handle: number) => host.sync(packet(DROP, (writer) => writer.handle(handle)));
  /** A guest-built request whose trailers come from a fresh writable future. */
  const request = () => {
    const trailers = futureNew(TRAILERS);
    const [handle] = sync(
      "request",
      "new",
      sync("fields", "constructor"),
      undefined,
      trailers,
      undefined,
    );
    return { request: handle as number, trailers };
  };
  /** Consume a guest body; returns the trailers future handed back to the guest. */
  const consume = (handle: number): number =>
    sync("request", "consumeBody", handle, futureNew(RES))[1];
  return { host, sync, fields, futureNew, write, read, drop, request, consume, callPacket };
}

async function retire(host: StandardHost): Promise<void> {
  host.endCallback();
  await host.settled();
  await host.dispose();
  expect(host.size).toBe(0);
  expect(host.pending).toBe(0);
}

test("FUTURE_NEW accepts only the generated writable future types", async () => {
  const { host, futureNew } = setup();
  expect(WRITABLE_FUTURES).toContain(TRAILERS);
  expect(WRITABLE_FUTURES).toContain(RES);
  const readable = TYPES.findIndex(
    (type, index) => type[0] === "future" && !WRITABLE_FUTURES.includes(index),
  );
  for (const type of [readable, 0, TYPES.length])
    expect(() => futureNew(type)).toThrow("unsupported writable future type");
  expect(() => host.sync(packet(FUTURE_NEW, (writer) => writer.u64(0n)))).toThrow("trailing");
  await retire(host);
});

test("FUTURE_WRITE validates the whole packet before claiming owned payload handles", async () => {
  const { host, sync, fields, futureNew, write, drop } = setup();
  const payload = TYPES[TRAILERS]![1] as number;
  const future = futureNew(TRAILERS);
  const owned = fields();
  const options = sync("request-options", "constructor");
  const stream = new Reader(
    host.sync(
      packet(STREAM_NEW, (writer) =>
        writer.u32(TYPES.findIndex(([kind, item]) => kind === "stream" && item === "u8")),
      ),
    ),
  ).handle();
  const before = host.size;
  // Trailing bytes, a handle of another resource and a dropped handle refuse the whole write.
  expect(() => write(future, payload, { tag: "ok", val: owned }, true)).toThrow("trailing");
  expect(() => write(future, payload, { tag: "ok", val: options })).toThrow("different type");
  const gone = fields();
  drop(gone);
  expect(() => write(future, payload, { tag: "ok", val: gone })).toThrow("unknown or dropped");
  // Only writable futures accept writes.
  expect(() => write(stream, payload, { tag: "ok" })).toThrow("not a live writable future");
  expect(() => write(123456789, payload, { tag: "ok" })).toThrow("not a live writable future");
  expect(host.size).toBe(before);
  expect(sync("fields", "has", owned, "x-a")).toBe(true);
  // The accepted write moves the owned trailers; the write is one-shot.
  expect(write(future, payload, { tag: "ok", val: owned }).length).toBe(0);
  expect(() => sync("fields", "has", owned, "x-a")).toThrow("unknown or dropped");
  expect(() => write(future, payload, { tag: "ok" })).toThrow("already written");
  // Nobody took the written trailers: dropping the future releases them.
  const count = released;
  drop(future);
  expect(released).toBe(count + 1);
  await retire(host);
});

test("an unwritten writer resolves to the abandoned error on DROP and at callback end", async () => {
  const { host, futureNew, read, drop, request, consume } = setup();
  const dropped = request();
  const out = consume(dropped.request);
  drop(dropped.trailers);
  expect((await read(out, OUT_TRAILERS)).value).toEqual(ABANDONED);

  const ended = request();
  const later = consume(ended.request);
  host.endCallback();
  expect((await read(later, OUT_TRAILERS)).value).toEqual(ABANDONED);
  // The callback's writers are gone; their numbers name nothing.
  expect(() => drop(ended.trailers)).toThrow("unknown or dropped");
  // Untransferred writers created in a callback also end with it.
  const unused = futureNew(TRAILERS);
  host.endCallback();
  expect(() => drop(unused)).toThrow("unknown or dropped");
  await retire(host);
});

test("handle-bearing FUTURE_READ issues trailers once and releases late or unread values", async () => {
  const { host, sync, fields, write, read, drop, request, consume } = setup();
  const payload = TYPES[TRAILERS]![1] as number;

  // Delivered: the trailers become a new guest handle carried by the reply journal.
  const delivered = request();
  const out = consume(delivered.request);
  write(delivered.trailers, payload, { tag: "ok", val: fields("x-t") });
  const { value, issued } = await read(out, OUT_TRAILERS);
  expect(issued).toEqual([value.val]);
  expect(sync("fields", "has", value.val, "x-t")).toBe(true);
  expect(() => drop(out)).toThrow("unknown or dropped");

  // A cancelled read consumes the future; the value written later is released, not issued.
  const cancelled = request();
  const pending = consume(cancelled.request);
  const callback = new AbortController();
  const reading = read(pending, OUT_TRAILERS, callback.signal);
  await tick();
  callback.abort();
  await expect(reading).rejects.toThrow("cancelled");
  const size = host.size;
  let count = released;
  write(cancelled.trailers, payload, { tag: "ok", val: fields() });
  await tick();
  expect(released).toBe(count + 1);
  // The fields handle created for the write moved into it; no new handle was issued.
  expect(host.size).toBe(size);

  // An unread future the guest drops releases its value when it arrives.
  const unread = request();
  const ignored = consume(unread.request);
  drop(ignored);
  count = released;
  write(unread.trailers, payload, { tag: "ok", val: fields() });
  await tick();
  expect(released).toBe(count + 1);

  // A reply that never reached the guest cancels the futures it would have published.
  const undelivered = request();
  const lost = consume(undelivered.request);
  host.discard({ issued: [lost] });
  count = released;
  write(undelivered.trailers, payload, { tag: "ok", val: fields() });
  expect(released).toBe(count + 1);

  // A discarded FUTURE_READ reply rolls its issued trailers back.
  const rolled = request();
  const rolledOut = consume(rolled.request);
  write(rolled.trailers, payload, { tag: "ok", val: fields() });
  const delivery = await read(rolledOut, OUT_TRAILERS);
  count = released;
  host.discard({ issued: delivery.issued });
  expect(released).toBe(count + 1);
  expect(() => sync("fields", "has", delivery.value.val, "x-a")).toThrow("unknown or dropped");
  await retire(host);
});

test("an async owned parameter moves at the call's first turn; a cancelled call moves nothing", async () => {
  const { host, sync, request, callPacket } = setup();
  const aborted = new AbortController();
  aborted.abort();
  const kept = request();
  await expect(
    host.async(callPacket(undefined, "send", [kept.request]), aborted.signal),
  ).rejects.toThrow("cancelled");
  // The guest still owns the request and may use or drop it.
  expect(sync("request", "getMethod", kept.request)).toEqual({ tag: "get" });
  const moved = request();
  const delivery = await host.async(
    callPacket(undefined, "send", [moved.request]),
    new AbortController().signal,
  );
  expect(delivery.issued).toEqual([]);
  expect(() => sync("request", "getMethod", moved.request)).toThrow("unknown or dropped");
  // A moved handle cannot be named again in the same or a later packet.
  await expect(
    host.async(callPacket(undefined, "send", [moved.request]), new AbortController().signal),
  ).rejects.toThrow("unknown or dropped");
  await retire(host);
});
