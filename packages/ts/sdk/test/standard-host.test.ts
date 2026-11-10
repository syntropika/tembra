// Boundary tests of the browser standard WASI host. They send raw packets that the
// generated guest bindings never produce, so hostile handle numbers stay out of the example Orbs.
import { expect, test } from "bun:test";
import {
  type Descriptor,
  type DirectoryDescriptor,
  FilesystemError,
  memoryFilesystem,
  type MountedFilesystem,
} from "../src/interpreter/filesystem.ts";
import { METHODS, TYPES } from "../src/interpreter/standard.ts";
import {
  CALL,
  DROP,
  FUTURE_READ,
  type HandleCodec,
  Reader,
  STREAM_NEW,
  STREAM_READ,
  STREAM_WRITE,
  Writer,
  decode,
  encode,
  utf8Decode,
  utf8Encode,
} from "../src/interpreter/standard-codec.ts";
import { StandardHost } from "../src/interpreter/standard-host.ts";

const FS = "wasi:filesystem/types@0.3.0";
const PREOPENS = "wasi:filesystem/preopens@0.3.0";
const CLOCK = "wasi:clocks/monotonic-clock@0.3.0";
const RANDOM = "wasi:random/random@0.3.0";
const ALL = new Set([FS, PREOPENS, CLOCK, RANDOM]);
const BYTES = TYPES.findIndex(([kind, item]) => kind === "stream" && item === "u8");
/** A hostile guest names handles by raw number. */
const raw: HandleCodec = {
  handle: (_kind, _detail, value) => value as number,
  value: (_k, _d, h) => h,
};
const pattern = Uint8Array.from({ length: 4100 }, (_, index) => index % 256);
const tick = () => new Promise((resolve) => setTimeout(resolve, 0));

function deferred<T = void>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((accept) => {
    resolve = accept;
  });
  return { promise, resolve };
}

function index(name: string): number {
  const found = METHODS.findIndex((method) => method.name === name);
  if (found < 0) throw new Error(`${name} is not in the generated table`);
  return found;
}

function call(name: string, ...args: unknown[]): Uint8Array {
  const method = METHODS[index(name)]!;
  const writer = new Writer();
  writer.u8(0);
  writer.u8(CALL);
  writer.u32(index(name));
  method.params.forEach((type, position) => encode(writer, type, args[position], raw));
  return writer.finish();
}

function result(name: string, bytes: Uint8Array): any {
  const reader = new Reader(bytes);
  const value = decode(reader, METHODS[index(name)]!.result!, raw);
  reader.end();
  return value;
}

function operation(kind: number, handle: number, limit?: number): Uint8Array {
  const writer = new Writer();
  writer.u8(0);
  writer.u8(kind);
  writer.handle(handle);
  if (limit !== undefined) writer.u32(limit);
  return writer.finish();
}

function streamNew(type = BYTES): Uint8Array {
  const writer = new Writer();
  writer.u8(0);
  writer.u8(STREAM_NEW);
  writer.u32(type);
  return writer.finish();
}

function streamWrite(handle: number, chunk: Uint8Array): Uint8Array {
  const writer = new Writer();
  writer.u8(0);
  writer.u8(STREAM_WRITE);
  writer.handle(handle);
  writer.bytes(chunk);
  return writer.finish();
}

const mount = (filesystem: MountedFilesystem, path = "/data") => ({
  path,
  writable: filesystem.writable,
  filesystem,
});

function host(filesystem: MountedFilesystem, allowed = ALL): StandardHost {
  return new StandardHost(allowed, [mount(filesystem)], 8 << 20);
}

/** Wrap provider descriptors to count calls and closes and to gate chosen methods. */
function instrument(
  base: MountedFilesystem,
  gate: (method: string, args: unknown[]) => Promise<void> | void = () => {},
) {
  const stats = { calls: [] as string[], closes: 0, returns: 0 };
  const originals = new WeakMap<object, Descriptor>();
  const methods = [
    "stat",
    "isSameObject",
    "sync",
    "read",
    "write",
    "append",
    "setSize",
    "openAt",
    "statAt",
    "createDirectoryAt",
    "removeDirectoryAt",
    "unlinkFileAt",
    "renameAt",
  ];
  const wrap = (descriptor: Descriptor): Descriptor => {
    const wrapper: Record<string, unknown> = {
      type: descriptor.type,
      get writable() {
        return descriptor.writable;
      },
      close() {
        stats.closes++;
        descriptor.close();
      },
    };
    const target = descriptor as unknown as Record<string, (...args: unknown[]) => unknown>;
    for (const key of methods) {
      if (typeof target[key] !== "function") continue;
      wrapper[key] = async (...args: unknown[]) => {
        stats.calls.push(key);
        args = args.map((arg) => (arg && typeof arg === "object" && originals.get(arg)) || arg);
        await gate(key, args);
        const out = await target[key]!.apply(descriptor, args);
        return out && typeof out === "object" && "close" in out ? wrap(out as Descriptor) : out;
      };
    }
    if (descriptor.type === "directory")
      wrapper.readDirectory = () => {
        stats.calls.push("readDirectory");
        const entries = descriptor.readDirectory();
        return {
          next: () => entries.next(),
          return: (value?: unknown) => {
            stats.returns++;
            return entries.return!(value);
          },
          [Symbol.asyncIterator]() {
            return this;
          },
        };
      };
    const proxy = wrapper as unknown as Descriptor;
    originals.set(proxy, descriptor);
    return proxy;
  };
  const filesystem: MountedFilesystem = {
    writable: base.writable,
    openRoot: () => wrap(base.openRoot()) as DirectoryDescriptor,
    dispose: () => base.dispose(),
  };
  return { filesystem, stats };
}

/** Treat a direct host test's returned bytes as immediately delivered to its simulated guest. */
async function asynchronous(
  standard: StandardHost,
  packet: Uint8Array,
  signal?: AbortSignal,
): Promise<Uint8Array> {
  return (await standard.async(packet, signal)).bytes;
}

function roots(standard: StandardHost): number[] {
  return result("getDirectories", standard.sync(call("getDirectories"))).map(
    ([root]: [number]) => root,
  );
}

async function openAt(
  standard: StandardHost,
  directory: number,
  path: string,
  open: object = {},
  flags: object = { read: true },
): Promise<any> {
  return result(
    "openAt",
    await asynchronous(standard, call("openAt", directory, {}, path, open, flags)),
  );
}

async function open(standard: StandardHost, path = "bytes.bin", open = {}, flags = {}) {
  const opened = await openAt(standard, roots(standard)[0]!, path, open, { read: true, ...flags });
  expect(opened.tag).toBe("ok");
  return opened.val as number;
}

/** Drain a byte stream with small reads, then read its completion future. */
async function drain(standard: StandardHost, file: number, offset: bigint, limit = 1000) {
  const [stream, future] = result(
    "readViaStream",
    standard.sync(call("readViaStream", file, offset)),
  );
  const chunks: Uint8Array[] = [];
  for (;;) {
    const reader = new Reader(await asynchronous(standard, operation(STREAM_READ, stream, limit)));
    if (reader.u8() === 1) break;
    chunks.push(reader.bytes());
    reader.end();
  }
  const status = new Reader(await asynchronous(standard, operation(FUTURE_READ, future))).u8();
  const bytes = new Uint8Array(chunks.reduce((total, chunk) => total + chunk.length, 0));
  let at = 0;
  for (const chunk of chunks) bytes.set(chunk, (at += chunk.length) - chunk.length);
  standard.sync(operation(DROP, stream));
  return { bytes, chunks: chunks.length, done: status === 0 ? "ok" : "err" };
}

/** Write `chunks` through STREAM_NEW/STREAM_WRITE/DROP into write- or append-via-stream. */
async function write(
  standard: StandardHost,
  file: number,
  chunks: Uint8Array[],
  offset?: bigint,
): Promise<any> {
  const writer = new Reader(standard.sync(streamNew())).handle();
  const future = result(
    offset === undefined ? "appendViaStream" : "writeViaStream",
    standard.sync(
      offset === undefined
        ? call("appendViaStream", file, writer)
        : call("writeViaStream", file, writer, offset),
    ),
  );
  // A consumer that stopped (for example a refused descriptor) fails later writes.
  for (const chunk of chunks)
    await asynchronous(standard, streamWrite(writer, chunk)).catch((error) =>
      expect(String(error)).toContain("stream reader was dropped"),
    );
  standard.sync(operation(DROP, writer));
  const reader = new Reader(await asynchronous(standard, operation(FUTURE_READ, future)));
  const payload = TYPES[METHODS[index("writeViaStream")]!.result!]![1] as number;
  const value = decode(reader, payload, raw);
  reader.end();
  return value;
}

test("the codec writer keeps values written across buffer growth", () => {
  const writer = new Writer();
  for (let value = 0; value < 40; value++) writer.u32(value);
  writer.bytes(pattern);
  writer.u64(2n ** 64n - 1n);
  const reader = new Reader(writer.finish());
  for (let value = 0; value < 40; value++) expect(reader.u32()).toBe(value);
  expect(reader.bytes()).toEqual(pattern);
  expect(reader.u64()).toBe(2n ** 64n - 1n);
  reader.end();
});

test("host identities retain all safe integer bits and refuse rounded identities", () => {
  const writer = new Writer();
  writer.handle(2 ** 40 + 17);
  writer.handle(Number.MAX_SAFE_INTEGER);
  const reader = new Reader(writer.finish());
  expect(reader.handle()).toBe(2 ** 40 + 17);
  expect(reader.handle()).toBe(Number.MAX_SAFE_INTEGER);
  expect(() => writer.handle(Number.MAX_SAFE_INTEGER + 1)).toThrow("invalid handle");
  const forged = new Writer();
  forged.u64(2n ** 53n + 1n);
  expect(() => new Reader(forged.finish()).handle()).toThrow("invalid handle");
});

test("the codec is exact: no wrapping, unknown flag bits, enum cases or tags", () => {
  const find = (predicate: (type: readonly unknown[]) => boolean) => TYPES.findIndex(predicate);
  const u64 = find(([kind]) => kind === "u64");
  const flags = find(([kind, names]) => kind === "flags" && (names as string[]).length === 6);
  const option = find(([kind]) => kind === "option");
  const roundtrip = (type: number, value: unknown) => {
    const writer = new Writer();
    encode(writer, type, value, raw);
    const reader = new Reader(writer.finish());
    const decoded = decode(reader, type, raw);
    reader.end();
    return decoded;
  };
  expect(roundtrip(u64, 2n ** 64n - 1n)).toBe(2n ** 64n - 1n);
  expect(() => roundtrip(u64, 2n ** 64n)).toThrow(RangeError);
  expect(() => roundtrip(u64, -1n)).toThrow(RangeError);
  expect(() => roundtrip(u64, 0.5)).toThrow(RangeError);
  const bad = (type: number, bytes: number[]) => () => {
    const reader = new Reader(Uint8Array.from(bytes));
    decode(reader, type, raw);
  };
  expect(bad(flags, [0x40, 0, 0, 0])).toThrow("unknown flag bits");
  expect(roundtrip(flags, { read: true, mutateDirectory: true })).toMatchObject({
    read: true,
    write: false,
    mutateDirectory: true,
  });
  expect(bad(option, [2])).toThrow("option tag");
  const text = "aé€\u{1f600}";
  expect(utf8Decode(utf8Encode(text))).toBe(text);
  expect(utf8Encode("\ud800")).toEqual(Uint8Array.from([0xef, 0xbf, 0xbd]));
  for (const invalid of [
    [0xc0, 0x80],
    [0xed, 0xa0, 0x80],
    [0xf4, 0x90, 0x80, 0x80],
    [0xe2, 0x82],
  ])
    expect(() => utf8Decode(Uint8Array.from(invalid))).toThrow("invalid UTF-8");
});

test("filesystem shapes roundtrip: new-timestamp, advice, signed instants, metadata hashes", () => {
  const types = (name: string) => METHODS[index(name)]!;
  const timestamp = types("setTimes").params[1]!;
  const advice = types("advise").params[3]!;
  const hash = types("metadataHash").result!;
  const roundtrip = (type: number, value: unknown) => {
    const writer = new Writer();
    encode(writer, type, value, raw);
    const reader = new Reader(writer.finish());
    const decoded = decode(reader, type, raw);
    reader.end();
    return decoded;
  };
  for (const value of [
    { tag: "no-change" },
    { tag: "now" },
    { tag: "timestamp", val: { seconds: -(2n ** 63n), nanoseconds: 999_999_999 } },
    { tag: "timestamp", val: { seconds: 2n ** 63n - 1n, nanoseconds: 0 } },
  ])
    expect(roundtrip(timestamp, value)).toEqual(value);
  expect(() =>
    roundtrip(timestamp, { tag: "timestamp", val: { seconds: 2n ** 63n, nanoseconds: 0 } }),
  ).toThrow(RangeError);
  expect(roundtrip(advice, "no-reuse")).toBe("no-reuse");
  expect(() => roundtrip(advice, "never")).toThrow("unknown enum");
  const reader = new Reader(Uint8Array.from([6, 0, 0, 0]));
  expect(() => decode(reader, advice, raw)).toThrow("unknown enum");
  const exact = { lower: 2n ** 64n - 1n, upper: 0x0123_4567_89ab_cdefn };
  expect(roundtrip(hash, { tag: "ok", val: exact })).toEqual({ tag: "ok", val: exact });
});

test("binary reads stream raw chunks at exact u64 offsets", async () => {
  const standard = host(memoryFilesystem({ "bytes.bin": pattern }).open("/data", false));
  const file = await open(standard);
  const all = await drain(standard, file, 0n);
  expect(all.bytes).toEqual(pattern);
  expect(all.chunks).toBe(5);
  expect(all.done).toBe("ok");
  expect((await drain(standard, file, 4096n)).bytes).toEqual(pattern.subarray(4096));
  for (const offset of [4100n, (1n << 53n) + 17n, 2n ** 64n - 1n]) {
    const beyond = await drain(standard, file, offset);
    expect(beyond.bytes.length).toBe(0);
  }
  const stat = result("stat", await asynchronous(standard, call("stat", file)));
  expect(stat.val.size).toBe(4100n);
  expect(stat.val.type.tag).toBe("regular-file");
  await standard.dispose();
});

test("random and stream replies include their framing within the packet bound", async () => {
  const filesystem = memoryFilesystem({ "bytes.bin": pattern }).open("/data", false);
  const standard = new StandardHost(ALL, [mount(filesystem)], 64);
  expect(standard.sync(call("getRandomBytes", 1000n))).toHaveLength(64);
  const file = await open(standard);
  const [stream] = result("readViaStream", standard.sync(call("readViaStream", file, 0n)));
  expect(await asynchronous(standard, operation(STREAM_READ, stream, 65536))).toHaveLength(64);
  await standard.dispose();
});

test("forged, dropped, foreign, mistyped and unimported handles never reach a provider", async () => {
  const provider = memoryFilesystem({ "bytes.bin": pattern });
  const { filesystem, stats } = instrument(provider.open("/data", false));
  const standard = host(filesystem);
  const peer = host(provider.open("/data", false));
  const file = await open(standard);
  const foreign = await open(peer);
  const before = stats.calls.length;
  await expect(asynchronous(standard, call("stat", 0x7fff_ffff))).rejects.toThrow(
    "unknown or dropped",
  );
  await expect(asynchronous(standard, call("stat", foreign))).rejects.toThrow("unknown or dropped");
  const [stream] = result("readViaStream", standard.sync(call("readViaStream", file, 0n)));
  await expect(asynchronous(standard, call("stat", stream))).rejects.toThrow("different type");
  await expect(asynchronous(standard, operation(STREAM_READ, file, 16))).rejects.toThrow(
    "output stream",
  );
  expect(standard.sync(operation(DROP, file))).toEqual(new Uint8Array());
  expect(() => standard.sync(operation(DROP, file))).toThrow("unknown or dropped");
  await expect(asynchronous(standard, call("stat", file))).rejects.toThrow("unknown or dropped");
  expect(() => standard.sync(call("stat", foreign))).toThrow("wrong calling convention");
  expect(() => standard.sync(new Uint8Array([0, CALL, 99, 0, 0, 0]))).toThrow("unknown standard");
  const trailing = new Uint8Array([...call("getRandomU64"), 0]);
  expect(() => standard.sync(trailing)).toThrow("trailing");
  expect(stats.calls.length).toBe(before);
  const random = host(provider.open("/data", false), new Set([RANDOM]));
  expect(() => random.sync(call("getDirectories"))).toThrow("not imported");
  await expect(asynchronous(random, call("waitFor", 1n))).rejects.toThrow("not imported");
  expect(result("getRandomBytes", random.sync(call("getRandomBytes", 32n)))).toHaveLength(32);
  await Promise.all([standard.dispose(), peer.dispose(), random.dispose()]);
  expect(stats.closes).toBe(2);
});

test("traversal and open authority are refused before the provider is called", async () => {
  for (const writable of [false, true]) {
    const { filesystem, stats } = instrument(
      memoryFilesystem({ file: "x" }).open("/data", writable),
    );
    const standard = host(filesystem);
    const [root] = roots(standard);
    const attempt = async (path: string, open: object, flags: object) =>
      (await openAt(standard, root!, path, open, flags)).val?.tag;
    expect(await attempt("../escape", {}, { read: true })).toBe("not-permitted");
    expect(await attempt("a/../../escape", {}, { read: true })).toBe("not-permitted");
    expect(await attempt("/absolute", {}, { read: true })).toBe("not-permitted");
    expect(await attempt("nul\0", {}, { read: true })).toBe("invalid");
    if (!writable) {
      expect(await attempt("file", {}, { write: true })).toBe("not-permitted");
      expect(await attempt("file", { create: true }, { read: true })).toBe("not-permitted");
      expect(await attempt("file", { truncate: true }, { read: true })).toBe("not-permitted");
      expect(await attempt("dir", {}, { mutateDirectory: true })).toBe("not-permitted");
    }
    expect(stats.calls).toEqual([]);
    await standard.dispose();
  }
});

test("read-only directories, files and both rename/link sides refuse before any provider call", async () => {
  const provider = memoryFilesystem({ "a/file": "x", "b/other": "y" });
  const read = instrument(provider.open("/ro", false));
  const write = instrument(provider.open("/rw", true));
  const standard = new StandardHost(
    ALL,
    [mount(read.filesystem, "/ro"), mount(write.filesystem, "/rw")],
    8 << 20,
  );
  const [ro, rw] = roots(standard) as [number, number];
  const tag = async (name: string, ...args: unknown[]) =>
    result(name, await asynchronous(standard, call(name, ...args))).val?.tag;
  const nothing = { tag: "no-change" };
  for (const [name, args] of [
    ["createDirectoryAt", [ro, "c"]],
    ["removeDirectoryAt", [ro, "a"]],
    ["unlinkFileAt", [ro, "a/file"]],
    ["renameAt", [ro, "a/file", rw, "moved"]],
    ["renameAt", [rw, "b/other", ro, "moved"]],
    ["linkAt", [ro, {}, "a/file", rw, "linked"]],
    ["linkAt", [rw, {}, "b/other", ro, "linked"]],
    ["symlinkAt", [ro, "a/file", "link"]],
    ["setTimesAt", [ro, {}, "a/file", nothing, nothing]],
    ["setTimes", [ro, { tag: "now" }, nothing]],
  ] as [string, unknown[]][])
    expect([name, await tag(name, ...args)]).toEqual([name, "not-permitted"]);
  expect(read.stats.calls).toEqual([]);
  expect(write.stats.calls).toEqual([]);
  // A file opened without write right refuses writes and truncation as upstream does.
  const file = (await openAt(standard, ro, "a/file")).val as number;
  expect(await tag("setSize", file, 0n)).toBe("bad-descriptor");
  expect(await write_(standard, file)).toEqual({ tag: "err", val: { tag: "bad-descriptor" } });
  // Writable mounts report the browser's missing links as unsupported, after the checks.
  expect(await tag("linkAt", rw, {}, "b/other", rw, "linked")).toBe("unsupported");
  expect(await tag("symlinkAt", rw, "b/other", "link")).toBe("unsupported");
  expect(await tag("setTimes", rw, { tag: "now" }, nothing)).toBe("unsupported");
  expect(
    result("setTimes", await asynchronous(standard, call("setTimes", rw, nothing, nothing))).tag,
  ).toBe("ok");
  expect(await tag("readlinkAt", ro, "a/file")).toBe("invalid");
  expect(await tag("readlinkAt", ro, "missing")).toBe("no-entry");
  expect(
    result("advise", await asynchronous(standard, call("advise", file, 0n, 10n, "sequential"))).tag,
  ).toBe("ok");
  await standard.dispose();
});

/** A write into a read-only file through a fresh writer. */
function write_(standard: StandardHost, file: number) {
  return write(standard, file, [Uint8Array.of(1)], 0n);
}

test("a child directory descriptor cannot traverse to a sibling within its mount", async () => {
  const standard = host(
    memoryFilesystem({ "bytes.bin": pattern, "child/inside.bin": pattern }).open("/data", false),
  );
  const child = await open(standard, "child");
  expect(await openAt(standard, child, "../bytes.bin")).toEqual({
    tag: "err",
    val: { tag: "not-permitted" },
  });
  expect((await openAt(standard, child, "inside.bin")).tag).toBe("ok");
  await standard.dispose();
});

test("binary offset writes, appends, truncation and reads keep exact bytes", async () => {
  const standard = host(memoryFilesystem().open("/data", true));
  const file = await open(standard, "data.bin", { create: true }, { write: true });
  const binary = Uint8Array.from({ length: 150_000 }, (_, at) => (at * 7 + 0x80) % 256);
  const chunks = [0, 40_000, 80_000, 120_000].map((at) => binary.subarray(at, at + 40_000));
  expect(await write(standard, file, chunks, 3n)).toEqual({ tag: "ok" });
  expect(await write(standard, file, [Uint8Array.of(0xff, 0xfe)])).toEqual({ tag: "ok" });
  const read = await drain(standard, file, 0n, 65536);
  expect(read.bytes.length).toBe(150_005);
  expect(read.bytes.subarray(0, 3)).toEqual(new Uint8Array(3));
  expect(read.bytes.subarray(3, 150_003)).toEqual(binary);
  expect(read.bytes.subarray(150_003)).toEqual(Uint8Array.of(0xff, 0xfe));
  expect(result("setSize", await asynchronous(standard, call("setSize", file, 5n))).tag).toBe("ok");
  expect((await drain(standard, file, 0n)).bytes).toEqual(
    Uint8Array.of(0, 0, 0, binary[0]!, binary[1]!),
  );
  const stat = result("stat", await asynchronous(standard, call("stat", file))).val;
  expect([stat.size, stat.linkCount, stat.type.tag]).toEqual([5n, 1n, "regular-file"]);
  await standard.dispose();
  expect(standard.size).toBe(0);
});

test("typed directory streams deliver whole entries in bounded batches and close on drop", async () => {
  const files: Record<string, string> = { "sub/x": "" };
  for (let at = 0; at < 9; at++) files[`f${at}`] = "x";
  const { filesystem, stats } = instrument(memoryFilesystem(files).open("/data", false));
  const standard = host(filesystem);
  const [root] = roots(standard);
  const [stream, future] = result("readDirectory", standard.sync(call("readDirectory", root)));
  const pair = TYPES[METHODS[index("readDirectory")]!.result!]![1] as number[];
  const item = TYPES[pair[0]!]![1] as number;
  const names: string[] = [];
  let batches = 0;
  for (;;) {
    const reader = new Reader(await asynchronous(standard, operation(STREAM_READ, stream, 4)));
    if (reader.u8() === 1) break;
    const count = reader.u32();
    expect(count).toBeLessThanOrEqual(4);
    for (let at = 0; at < count; at++) {
      const entry = decode(reader, item, raw);
      names.push(`${entry.name}:${entry.type.tag}`);
    }
    reader.end();
    batches++;
  }
  expect(batches).toBe(3);
  expect(names.sort()).toContain("sub:directory");
  expect(names).toContain("f0:regular-file");
  expect(new Reader(await asynchronous(standard, operation(FUTURE_READ, future))).u8()).toBe(0);
  standard.sync(operation(DROP, stream));
  expect(stats.returns).toBe(0);
  // A stream dropped mid-iteration returns the provider iterator and fails its pending read.
  const [early] = result("readDirectory", standard.sync(call("readDirectory", root)));
  await asynchronous(standard, operation(STREAM_READ, early, 1));
  standard.sync(operation(DROP, early));
  await standard.settled();
  expect(stats.returns).toBe(1);
  // A file is not a directory: the stream ends and the future reports it.
  const file = (await openAt(standard, root!, "f0")).val;
  const [none, failed] = result("readDirectory", standard.sync(call("readDirectory", file)));
  expect(new Reader(await asynchronous(standard, operation(STREAM_READ, none, 1))).u8()).toBe(1);
  const reader = new Reader(await asynchronous(standard, operation(FUTURE_READ, failed)));
  expect([reader.u8(), reader.u32()]).toEqual([1, 23]);
  await standard.dispose();
});

test("directory create, rename, unlink, remove, same-object and metadata hashes", async () => {
  const provider = memoryFilesystem({ "a.txt": "one" });
  const standard = host(provider.open("/data", true));
  const [root] = roots(standard) as [number];
  const ok = async (name: string, ...args: unknown[]) =>
    result(name, await asynchronous(standard, call(name, ...args)));
  expect(await ok("createDirectoryAt", root, "dir")).toEqual({ tag: "ok" });
  expect((await ok("createDirectoryAt", root, "dir")).val.tag).toBe("exist");
  const held = (await openAt(standard, root, "a.txt")).val;
  expect(await ok("renameAt", root, "a.txt", root, "dir/b.txt")).toEqual({ tag: "ok" });
  // The descriptor follows its node, not a stale path string.
  expect(utf8Decode((await drain(standard, held, 0n)).bytes)).toBe("one");
  const moved = (await openAt(standard, root, "dir/b.txt")).val;
  expect(await asynchronous(standard, call("isSameObject", held, moved))).toEqual(Uint8Array.of(1));
  expect(await asynchronous(standard, call("isSameObject", held, root))).toEqual(Uint8Array.of(0));
  const first = (await ok("metadataHash", held)).val;
  expect(await ok("metadataHashAt", root, {}, "dir/b.txt")).toEqual({ tag: "ok", val: first });
  expect(typeof first.lower).toBe("bigint");
  expect((await ok("removeDirectoryAt", root, "dir")).val.tag).toBe("not-empty");
  expect(await ok("unlinkFileAt", root, "dir/b.txt")).toEqual({ tag: "ok" });
  expect(await ok("removeDirectoryAt", root, "dir")).toEqual({ tag: "ok" });
  expect((await ok("statAt", root, {}, "dir")).val.tag).toBe("no-entry");
  expect((await ok("getType", held)).val.tag).toBe("regular-file");
  expect((await ok("getFlags", root)).val).toMatchObject({ read: true, mutateDirectory: true });
  await standard.dispose();
});

test("input streams bound host buffering at 64 KiB with whole-chunk backpressure", async () => {
  const gate = deferred();
  let writes = 0;
  const { filesystem } = instrument(memoryFilesystem().open("/data", true), async (method) => {
    if (method === "write" && writes++ === 0) await gate.promise;
  });
  const standard = host(filesystem);
  const file = await open(standard, "big.bin", { create: true }, { write: true });
  const writer = new Reader(standard.sync(streamNew())).handle();
  const oversized = new Uint8Array((64 << 10) + 1);
  await expect(asynchronous(standard, streamWrite(writer, oversized))).rejects.toThrow("64 KiB");
  const future = result("writeViaStream", standard.sync(call("writeViaStream", file, writer, 0n)));
  const chunk = new Uint8Array(40 << 10).fill(7);
  await asynchronous(standard, streamWrite(writer, chunk));
  await tick();
  // The consumer holds the first chunk in a gated provider write; the buffer takes another.
  await asynchronous(standard, streamWrite(writer, chunk));
  let accepted = false;
  const third = asynchronous(standard, streamWrite(writer, chunk)).then(() => {
    accepted = true;
  });
  await tick();
  expect(accepted).toBe(false);
  await expect(asynchronous(standard, streamWrite(writer, chunk))).rejects.toThrow(
    "already pending",
  );
  gate.resolve();
  await third;
  standard.sync(operation(DROP, writer));
  expect(new Reader(await asynchronous(standard, operation(FUTURE_READ, future))).u8()).toBe(0);
  const stat = result("stat", await asynchronous(standard, call("stat", file))).val;
  expect(stat.size).toBe(3n * BigInt(40 << 10));
  await standard.dispose();
});

test("a failed consumer drops its reader; later writes fail and the future reports the error", async () => {
  const { filesystem } = instrument(memoryFilesystem().open("/data", true), (method) => {
    if (method === "write") throw new FilesystemError("insufficient-space");
  });
  const standard = host(filesystem);
  const file = await open(standard, "full.bin", { create: true }, { write: true });
  const writer = new Reader(standard.sync(streamNew())).handle();
  const future = result("writeViaStream", standard.sync(call("writeViaStream", file, writer, 0n)));
  await asynchronous(standard, streamWrite(writer, Uint8Array.of(1, 2, 3)));
  const reader = new Reader(await asynchronous(standard, operation(FUTURE_READ, future)));
  expect([reader.u8(), reader.u32()]).toEqual([1, 22]);
  await expect(asynchronous(standard, streamWrite(writer, Uint8Array.of(4)))).rejects.toThrow(
    "stream reader was dropped",
  );
  standard.sync(operation(DROP, writer));
  await standard.dispose();
});

test("stream transfer is transactional; a refused call leaves the writer reusable", async () => {
  const standard = host(memoryFilesystem().open("/data", true));
  const file = await open(standard, "t.bin", { create: true }, { write: true });
  const [root] = roots(standard);
  expect(() => standard.sync(streamNew(0))).toThrow("unsupported input stream type");
  const writer = new Reader(standard.sync(streamNew())).handle();
  const size = standard.size;
  expect(() =>
    standard.sync(new Uint8Array([...call("writeViaStream", file, writer, 0n), 0])),
  ).toThrow("trailing");
  expect(() => standard.sync(call("writeViaStream", 0x7fff_fff0, writer, 0n))).toThrow("unknown");
  expect(() => standard.sync(call("writeViaStream", writer, writer, 0n))).toThrow("different type");
  expect(() => standard.sync(call("writeViaStream", file, root, 0n))).toThrow("different type");
  expect(standard.size).toBe(size);
  // Bytes written before transfer are kept and delivered after it.
  await asynchronous(standard, streamWrite(writer, utf8Encode("early ")));
  const future = result("writeViaStream", standard.sync(call("writeViaStream", file, writer, 0n)));
  expect(() => standard.sync(call("appendViaStream", file, writer))).toThrow("already transferred");
  await asynchronous(standard, streamWrite(writer, utf8Encode("late")));
  standard.sync(operation(DROP, writer));
  expect(new Reader(await asynchronous(standard, operation(FUTURE_READ, future))).u8()).toBe(0);
  expect(utf8Decode((await drain(standard, file, 0n)).bytes)).toBe("early late");
  // A never-transferred writer is discarded by DROP.
  const unused = new Reader(standard.sync(streamNew())).handle();
  await asynchronous(standard, streamWrite(unused, Uint8Array.of(1)));
  standard.sync(operation(DROP, unused));
  await expect(asynchronous(standard, streamWrite(unused, Uint8Array.of(1)))).rejects.toThrow(
    "not a live input",
  );
  await standard.dispose();
});

test("callback end closes writers it left open; pending writes abort and are not delivered", async () => {
  const gate = deferred();
  const entered = deferred();
  const { filesystem } = instrument(memoryFilesystem().open("/data", true), async (method) => {
    if (method === "write") {
      entered.resolve();
      await gate.promise;
    }
  });
  const standard = host(filesystem);
  const file = await open(standard, "c.bin", { create: true }, { write: true });
  const writer = new Reader(standard.sync(streamNew())).handle();
  const future = result("writeViaStream", standard.sync(call("writeViaStream", file, writer, 0n)));
  const controller = new AbortController();
  const full = new Uint8Array(64 << 10).fill(1);
  // The consumer holds the first chunk in a provider write; the buffer holds the second.
  await asynchronous(standard, streamWrite(writer, full), controller.signal);
  await entered.promise;
  await asynchronous(standard, streamWrite(writer, full), controller.signal);
  const pending = asynchronous(standard, streamWrite(writer, full), controller.signal);
  controller.abort();
  await expect(pending).rejects.toThrow("cancelled");
  standard.endCallback();
  expect(() => standard.sync(operation(DROP, writer))).toThrow("unknown or dropped");
  let settled = false;
  const physical = standard.settled().then(() => {
    settled = true;
  });
  await tick();
  expect(settled).toBe(false);
  gate.resolve();
  await physical;
  expect(new Reader(await asynchronous(standard, operation(FUTURE_READ, future))).u8()).toBe(0);
  const size = result("stat", await asynchronous(standard, call("stat", file))).val.size;
  expect(size).toBe(2n * BigInt(64 << 10));
  await standard.dispose();
});

test("cancellation settles pending reads, keeps descriptors and delivers the chunk later", async () => {
  const gate = deferred();
  const entered = deferred();
  const { filesystem, stats } = instrument(
    memoryFilesystem({ "bytes.bin": pattern }).open("/data", false),
    async (method) => {
      if (method === "read" && stats.calls.filter((name) => name === "read").length === 1) {
        entered.resolve();
        await gate.promise;
      }
    },
  );
  const standard = host(filesystem);
  const file = await open(standard);
  const controller = new AbortController();
  const waiting = asynchronous(standard, call("waitFor", 60_000_000_000n), controller.signal);
  const [stream] = result("readViaStream", standard.sync(call("readViaStream", file, 0n)));
  const reading = asynchronous(standard, operation(STREAM_READ, stream, 100), controller.signal);
  let physical = false;
  const settled = standard.settled().then(() => {
    physical = true;
  });
  await entered.promise;
  controller.abort();
  for (const outcome of await Promise.allSettled([waiting, reading]))
    expect(String((outcome as PromiseRejectedResult).reason)).toContain("cancelled");
  await tick();
  expect(physical).toBe(false);
  gate.resolve();
  await settled;
  // The produced chunk is kept for the next read: no byte is lost or repeated.
  const next = new Reader(await asynchronous(standard, operation(STREAM_READ, stream, 100)));
  expect(next.u8()).toBe(0);
  expect(next.bytes()).toEqual(pattern.subarray(0, 100));
  expect(stats.closes).toBe(0);
  // Retirement closes the root and the file.
  await standard.dispose();
  expect(stats.closes).toBe(2);
  expect(standard.size).toBe(0);
  expect(() => standard.sync(call("getDirectories"))).toThrow("closed");
});

test("async borrows refuse drop until the physical call returns, including cancellation", async () => {
  const gate = deferred();
  const entered = deferred();
  const { filesystem, stats } = instrument(
    memoryFilesystem({ "bytes.bin": pattern }).open("/data", false),
    async (method) => {
      if (method === "stat") {
        entered.resolve();
        await gate.promise;
      }
    },
  );
  const standard = host(filesystem);
  const file = await open(standard);
  const controller = new AbortController();
  const pending = asynchronous(standard, call("stat", file), controller.signal);
  await entered.promise;
  expect(() => standard.sync(operation(DROP, file))).toThrow("borrowed by a pending call");
  controller.abort();
  await expect(pending).rejects.toThrow("cancelled");
  // Cancellation does not end the loan; the provider call is still running.
  expect(() => standard.sync(operation(DROP, file))).toThrow("borrowed by a pending call");
  gate.resolve();
  await standard.settled();
  standard.sync(operation(DROP, file));
  expect(stats.closes).toBe(1);
  await standard.dispose();
});

test("several borrows in one call are each lent and released", async () => {
  const gate = deferred();
  const { filesystem, stats } = instrument(
    memoryFilesystem({ a: "x" }).open("/data", true),
    async (method) => {
      if (method === "renameAt" || method === "isSameObject") await gate.promise;
    },
  );
  const standard = host(filesystem);
  const [left, right] = roots(standard).concat(roots(standard)) as [number, number];
  const rename = asynchronous(standard, call("renameAt", left, "a", right, "b"));
  const same = asynchronous(standard, call("isSameObject", right, right));
  await tick();
  expect(() => standard.sync(operation(DROP, left))).toThrow("borrowed");
  expect(() => standard.sync(operation(DROP, right))).toThrow("borrowed");
  gate.resolve();
  expect(result("renameAt", await rename)).toEqual({ tag: "ok" });
  expect(await same).toEqual(Uint8Array.of(1));
  standard.sync(operation(DROP, left));
  standard.sync(operation(DROP, right));
  expect(stats.closes).toBe(2);
  await standard.dispose();
});

test("unpublished replies roll back: a full table and an undelivered open close descriptors", async () => {
  const { filesystem, stats } = instrument(
    memoryFilesystem({ "bytes.bin": pattern }).open("/data", false),
  );
  const standard = host(filesystem);
  const [root] = roots(standard);
  const delivery = await standard.async(call("openAt", root, {}, "bytes.bin", {}, { read: true }));
  expect(delivery.issued).toHaveLength(1);
  standard.discard(delivery);
  expect(stats.closes).toBe(1);
  const issued: number[] = [];
  try {
    for (;;) issued.push(...roots(standard));
  } catch (error) {
    expect(String(error)).toContain("table is full");
  }
  expect(standard.size).toBe(4096);
  // The refused call (a list reply) closed every root it had opened.
  expect(stats.closes).toBe(1 + 1);
  for (const handle of issued) standard.sync(operation(DROP, handle));
  await standard.dispose();
  expect(stats.closes).toBe(2 + issued.length + 1);
});

test("a stream keeps its file readable after the descriptor is dropped", async () => {
  const { filesystem, stats } = instrument(
    memoryFilesystem({ "bytes.bin": pattern }).open("/data", false),
  );
  const standard = host(filesystem);
  const file = await open(standard);
  const [stream] = result("readViaStream", standard.sync(call("readViaStream", file, 0n)));
  standard.sync(operation(DROP, file));
  expect(stats.closes).toBe(0);
  const reader = new Reader(await asynchronous(standard, operation(STREAM_READ, stream, 10)));
  expect(reader.u8()).toBe(0);
  expect(reader.bytes()).toEqual(pattern.subarray(0, 10));
  standard.sync(operation(DROP, stream));
  expect(stats.closes).toBe(1);
  await standard.dispose();
});

test("retirement waits for physical provider work and destructors", async () => {
  const gate = deferred();
  const entered = deferred();
  const { filesystem, stats } = instrument(
    memoryFilesystem({ "bytes.bin": pattern }).open("/data", false),
    async (method) => {
      if (method === "read") {
        entered.resolve();
        await gate.promise;
      }
    },
  );
  const standard = host(filesystem);
  const file = await open(standard);
  const [stream] = result("readViaStream", standard.sync(call("readViaStream", file, 0n)));
  const read = asynchronous(standard, operation(STREAM_READ, stream, 10)).catch(String);
  await entered.promise;
  let disposed = false;
  const disposal = standard.dispose().then(() => {
    disposed = true;
  });
  await tick();
  expect(disposed).toBe(false);
  expect(stats.closes).toBe(0);
  gate.resolve();
  await disposal;
  await read;
  expect(stats.closes).toBe(2);
  expect(standard.size).toBe(0);
});

test("unpublished byte and directory reads poison their streams instead of skipping data", async () => {
  for (const typed of [false, true]) {
    const standard = host(
      memoryFilesystem({ "bytes.bin": new Uint8Array(150_000) }).open("/data", false),
    );
    const file = await open(standard);
    const [stream, future] = typed
      ? result("readDirectory", standard.sync(call("readDirectory", roots(standard)[0]!)))
      : result("readViaStream", standard.sync(call("readViaStream", file, 0n)));
    const delivery = await standard.async(operation(STREAM_READ, stream, typed ? 1 : 65536));
    expect(delivery.bytes[0]).toBe(0);
    expect(delivery.reads).toEqual([stream]);
    standard.discard(delivery);
    await expect(standard.async(operation(STREAM_READ, stream, 1))).rejects.toThrow(
      "stream read was abandoned",
    );
    // A fresh stream on the retained descriptor still starts at the requested offset.
    expect((await drain(standard, file, 0n, 65536)).bytes.length).toBe(150_000);
    standard.sync(operation(DROP, stream));
    await asynchronous(standard, operation(FUTURE_READ, future));
    await standard.dispose();
    expect(standard.size).toBe(0);
  }
});

test("a synchronous directory-provider failure is reported through the standard future", async () => {
  const base = memoryFilesystem().open("/data", false);
  const standard = host({
    writable: false,
    openRoot() {
      const root = base.openRoot();
      return {
        ...root,
        readDirectory() {
          throw new FilesystemError("no-entry");
        },
      };
    },
    dispose: () => base.dispose(),
  });
  const [stream, future] = result(
    "readDirectory",
    standard.sync(call("readDirectory", roots(standard)[0]!)),
  );
  expect((await asynchronous(standard, operation(STREAM_READ, stream, 1)))[0]).toBe(1);
  expect((await asynchronous(standard, operation(FUTURE_READ, future)))[0]).toBe(1);
  await standard.dispose();
});
