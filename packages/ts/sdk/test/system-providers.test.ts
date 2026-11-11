import { expect, test } from "bun:test";
import { StandardHost } from "../src/interpreter/standard-host.ts";
import { METHODS, TYPES } from "../src/interpreter/standard.ts";
import {
  CALL,
  DROP,
  STREAM_NEW,
  STREAM_READ,
  STREAM_WRITE,
  type HandleCodec,
  Reader,
  Writer,
  decode,
  encode,
} from "../src/interpreter/standard-codec.ts";

const raw: HandleCodec = {
  handle: (_kind, _type, value) => value as number,
  value: (_kind, _type, handle) => handle,
};
const mono = "wasi:clocks/monotonic-clock@0.3.0",
  wall = "wasi:clocks/system-clock@0.3.0";
const stdin = "wasi:cli/stdin@0.3.0",
  stdout = "wasi:cli/stdout@0.3.0";
const tick = () => new Promise((resolve) => setTimeout(resolve, 0));
function deferred<T = void>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((accept) => (resolve = accept));
  return { promise, resolve };
}
function call(interface_: string, name: string, ...values: unknown[]) {
  const index = METHODS.findIndex(
    (method) => method.interface === interface_ && method.name === name,
  );
  if (index < 0) throw new Error(`Missing generated method ${interface_}#${name}`);
  const writer = new Writer();
  writer.u8(0);
  writer.u8(CALL);
  writer.u32(index);
  METHODS[index]!.params.forEach((type, position) => encode(writer, type, values[position], raw));
  return {
    packet: writer.finish(),
    result: (bytes: Uint8Array) => {
      const reader = new Reader(bytes);
      const value = decode(reader, METHODS[index]!.result!, raw);
      reader.end();
      return value;
    },
  };
}
function packet(operation: number, handle: number, tail?: (writer: Writer) => void) {
  const writer = new Writer();
  writer.u8(0);
  writer.u8(operation);
  writer.handle(handle);
  tail?.(writer);
  return writer.finish();
}

test("configured clocks are exact and denied imports never reach their provider", async () => {
  let calls = 0;
  const clocks = {
    systemNow() {
      calls++;
      return { seconds: -9007199254740993n, nanoseconds: 999999999 };
    },
    systemResolution: () => 17n,
    monotonicNow: () => 18446744073709551615n,
    monotonicResolution: () => 19n,
    async waitUntil(_when: bigint, signal?: AbortSignal) {
      await new Promise<void>((_resolve, reject) => {
        signal?.addEventListener("abort", () => reject(new Error("cancelled")), { once: true });
      });
    },
  };
  const denied = new StandardHost(new Set(), [], 1024, { clocks });
  const now = call(wall, "now");
  expect(() => denied.sync(now.packet)).toThrow("not imported");
  expect(calls).toBe(0);
  await denied.dispose();
  const host = new StandardHost(new Set([wall, mono]), [], 1024, { clocks });
  expect(now.result(host.sync(now.packet))).toEqual({
    seconds: -9007199254740993n,
    nanoseconds: 999999999,
  });
  const monotonic = call(mono, "now");
  expect(monotonic.result(host.sync(monotonic.packet))).toBe(18446744073709551615n);
  const stop = new AbortController(),
    wait = call(mono, "waitUntil", 123n);
  const pending = host.async(wait.packet, stop.signal);
  stop.abort();
  await expect(pending).rejects.toThrow("cancelled");
  await host.dispose();
  expect(host.size).toBe(0);
});

test("random providers have stable revision seeds and exact bounded byte replies", async () => {
  let seeds = 0;
  const random = {
    secure: (length: number) => new Uint8Array(length).fill(255),
    insecure: (length: number) => new Uint8Array(length).fill(128),
    insecureSeed() {
      seeds++;
      return [18446744073709551615n, 9007199254740993n] as const;
    },
  };
  const secure = "wasi:random/random@0.3.0",
    insecure = "wasi:random/insecure@0.3.0",
    seed = "wasi:random/insecure-seed@0.3.0";
  const host = new StandardHost(new Set([secure, insecure, seed]), [], 1024, { random });
  const get = call(seed, "getInsecureSeed");
  expect(get.result(host.sync(get.packet))).toEqual([18446744073709551615n, 9007199254740993n]);
  host.sync(get.packet);
  host.random.insecureSeed();
  expect(seeds).toBe(1);
  const capped = call(insecure, "getInsecureRandomBytes", 18446744073709551615n);
  expect(capped.result(host.sync(capped.packet))).toEqual(new Uint8Array(1020).fill(128));
  const word = call(secure, "getRandomU64");
  expect(word.result(host.sync(word.packet))).toBe(18446744073709551615n);
  await host.dispose();
  const invalid = new StandardHost(new Set([secure]), [], 1024, {
    random: { ...random, secure: () => new Uint8Array(1) },
  });
  expect(() => invalid.sync(word.packet)).toThrow("wrong byte length");
  await invalid.dispose();
});

test("retirement retains an uncancellable stdin read until its physical promise returns", async () => {
  const gate = deferred<Uint8Array | undefined>(),
    started = deferred();
  let signal: AbortSignal | undefined,
    closed = 0;
  const host = new StandardHost(new Set([stdin]), [], 1024, {
    stdio: {
      stdin: {
        read(_limit, active) {
          signal = active;
          started.resolve();
          return gate.promise;
        },
        close() {
          closed++;
        },
      },
    },
  });
  const open = call(stdin, "readViaStream"),
    [stream] = open.result(host.sync(open.packet)) as number[];
  const reading = host
    .async(packet(STREAM_READ, stream!, (writer) => writer.u32(16)))
    .catch(() => {});
  await started.promise;
  let retired = false;
  const retirement = host.dispose().then(() => {
    retired = true;
  });
  await tick();
  expect(signal?.aborted).toBe(true);
  expect(retired).toBe(false);
  expect(closed).toBe(0);
  gate.resolve(new Uint8Array([0, 255]));
  await reading;
  await retirement;
  expect(closed).toBe(1);
  expect(host.size).toBe(0);
});

test("binary stdout preserves backpressure and retirement waits for an admitted physical write", async () => {
  const gate = deferred(),
    started = deferred();
  let signal: AbortSignal | undefined,
    closed = 0,
    received: Uint8Array | undefined;
  const host = new StandardHost(new Set([stdout]), [], 1024, {
    stdio: {
      stdout: {
        async write(bytes, active) {
          signal = active;
          received = bytes;
          started.resolve();
          await gate.promise;
        },
        close() {
          closed++;
        },
      },
    },
  });
  const type = TYPES.findIndex(([kind, item]) => kind === "stream" && item === "u8");
  const create = new Writer();
  create.u8(0);
  create.u8(STREAM_NEW);
  create.u32(type);
  const stream = new Reader(host.sync(create.finish())).handle();
  host.sync(call(stdout, "writeViaStream", stream).packet);
  await host.async(
    packet(STREAM_WRITE, stream, (writer) => writer.bytes(new Uint8Array([0, 255, 128, 10]))),
  );
  await started.promise;
  host.sync(packet(DROP, stream));
  let retired = false;
  const retirement = host.dispose().then(() => {
    retired = true;
  });
  await tick();
  expect(signal?.aborted).toBe(true);
  expect(retired).toBe(false);
  expect(closed).toBe(0);
  gate.resolve();
  await retirement;
  expect(received).toEqual(new Uint8Array([0, 255, 128, 10]));
  expect(closed).toBe(1);
  expect(host.size).toBe(0);
});

test("injected clocks refuse invalid shapes and wait cancellation retains physical ownership", async () => {
  const gate = deferred(),
    started = deferred();
  const clocks = {
    systemNow: () => ({ seconds: 0n, nanoseconds: 1_000_000_000 }),
    systemResolution: () => -1n,
    monotonicNow: () => 1n << 64n,
    monotonicResolution: () => 1n,
    async waitUntil() {
      started.resolve();
      await gate.promise;
    },
  };
  const host = new StandardHost(new Set([wall, mono]), [], 1024, { clocks });
  expect(() => host.sync(call(wall, "now").packet)).toThrow("fractional nanoseconds");
  expect(() => host.sync(call(wall, "getResolution").packet)).toThrow("u64");
  expect(() => host.sync(call(mono, "now").packet)).toThrow("u64");
  const stop = new AbortController();
  const waiting = host.async(call(mono, "waitUntil", 1n).packet, stop.signal);
  await started.promise;
  stop.abort();
  await expect(waiting).rejects.toThrow("cancelled");
  let retired = false;
  const retirement = host.dispose().then(() => {
    retired = true;
  });
  await tick();
  expect(retired).toBe(false);
  gate.resolve();
  await retirement;
  expect(host.size).toBe(0);
});
