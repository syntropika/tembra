// Guest input streams through a real StandardHost into providers: one `writeStream` call per
// stream, the per-chunk fallback, the per-revision accepted-input budget, and physical
// ownership of provider work and asynchronous provider cleanup.
import { expect, test } from "bun:test";
import {
  type Descriptor,
  type DirectoryDescriptor,
  type FileDescriptor,
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
  WRITE_CLOSED,
} from "../src/interpreter/standard-codec.ts";
import { StandardHost } from "../src/interpreter/standard-host.ts";
import { INPUT_BUDGET } from "../src/interpreter/standard-streams.ts";

const ALL = new Set(["wasi:filesystem/types@0.3.0", "wasi:filesystem/preopens@0.3.0"]);
const BYTES = TYPES.findIndex(([kind, item]) => kind === "stream" && item === "u8");
const CHUNK = 64 << 10;
const raw: HandleCodec = { handle: (_k, _d, value) => value as number, value: (_k, _d, h) => h };
const tick = () => new Promise((resolve) => setTimeout(resolve, 0));

function deferred<T = void>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((accept) => {
    resolve = accept;
  });
  return { promise, resolve };
}

const index = (name: string) => METHODS.findIndex((method) => method.name === name);

function call(name: string, ...args: unknown[]): Uint8Array {
  const writer = new Writer();
  writer.u8(0);
  writer.u8(CALL);
  writer.u32(index(name));
  METHODS[index(name)]!.params.forEach((type, at) => encode(writer, type, args[at], raw));
  return writer.finish();
}

function result(name: string, bytes: Uint8Array): any {
  const reader = new Reader(bytes);
  const value = decode(reader, METHODS[index(name)]!.result!, raw);
  reader.end();
  return value;
}

function packet(operation: number, handle: number, tail?: (writer: Writer) => void): Uint8Array {
  const writer = new Writer();
  writer.u8(0);
  writer.u8(operation);
  writer.handle(handle);
  tail?.(writer);
  return writer.finish();
}

const write = (handle: number, chunk: Uint8Array) =>
  packet(STREAM_WRITE, handle, (writer) => writer.bytes(chunk));

function writerOf(standard: StandardHost): number {
  const writer = new Writer();
  writer.u8(0);
  writer.u8(STREAM_NEW);
  writer.u32(BYTES);
  return new Reader(standard.sync(writer.finish())).handle();
}

/** Payload of a write future: `result<_, error-code>` as `{ tag, val }`. */
async function settle(standard: StandardHost, future: number) {
  const reader = new Reader((await standard.async(packet(FUTURE_READ, future))).bytes);
  const payload = TYPES[METHODS[index("writeViaStream")]!.result!]![1] as number;
  return decode(reader, payload, raw);
}

async function readAll(standard: StandardHost, file: number): Promise<Uint8Array> {
  const [stream] = result("readViaStream", standard.sync(call("readViaStream", file, 0n)));
  const parts: number[] = [];
  for (;;) {
    const reader = new Reader(
      (await standard.async(packet(STREAM_READ, stream, (w) => w.u32(CHUNK)))).bytes,
    );
    if (reader.u8() === 1) break;
    parts.push(...reader.bytes());
  }
  standard.sync(packet(DROP, stream));
  return Uint8Array.from(parts);
}

interface Hooks {
  /** Run before each chunk a provider stream takes; may wait or throw. */
  chunk?(index: number): Promise<void> | void;
  /** Keep the provider's `writeStream` (default) or remove it to force the fallback. */
  stream?: boolean;
  /** Return value of `close()` for custom asynchronous cleanup. */
  close?(): Promise<void> | void;
  /** Return value of a directory iterator's `return()`. */
  returned?(): Promise<void> | void;
}

/** Wrap provider descriptors (providers freeze theirs) to observe calls and inject faults. */
function observed(base: MountedFilesystem, hooks: Hooks = {}) {
  const stats = {
    streams: [] as (bigint | undefined)[],
    sizes: [] as number[],
    writes: 0,
    closes: 0,
  };
  const wrap = (descriptor: Descriptor): Descriptor => {
    const target = descriptor as unknown as Record<string, unknown>;
    const wrapper: Record<string, unknown> = { type: descriptor.type };
    Object.defineProperty(wrapper, "writable", { get: () => descriptor.writable });
    for (const [key, value] of Object.entries(target))
      if (typeof value === "function") wrapper[key] = value.bind(descriptor);
    wrapper.close = () => {
      stats.closes++;
      descriptor.close();
      return hooks.close?.();
    };
    if (descriptor.type === "directory") {
      wrapper.openAt = async (...args: Parameters<DirectoryDescriptor["openAt"]>) =>
        wrap(await descriptor.openAt(...args));
      wrapper.readDirectory = () => {
        const entries = descriptor.readDirectory();
        return {
          next: () => entries.next(),
          return: async (value?: unknown) => {
            await hooks.returned?.();
            return entries.return!(value);
          },
          [Symbol.asyncIterator]() {
            return this;
          },
        };
      };
    } else {
      const file = descriptor as FileDescriptor;
      wrapper.write = (offset: bigint, data: Uint8Array) => {
        stats.writes++;
        return file.write(offset, data);
      };
      wrapper.append = (data: Uint8Array) => {
        stats.writes++;
        return file.append(data);
      };
      if (hooks.stream === false) delete wrapper.writeStream;
      else
        wrapper.writeStream = (chunks: AsyncIterable<Uint8Array>, offset?: bigint) => {
          stats.streams.push(offset);
          async function* checked() {
            for await (const chunk of chunks) {
              await hooks.chunk?.(stats.sizes.length);
              stats.sizes.push(chunk.byteLength);
              yield chunk;
            }
          }
          return file.writeStream!(checked(), offset);
        };
    }
    return wrapper as unknown as Descriptor;
  };
  const filesystem: MountedFilesystem = {
    writable: base.writable,
    openRoot: () => wrap(base.openRoot()) as DirectoryDescriptor,
    dispose: () => base.dispose(),
  };
  return { filesystem, stats };
}

async function setup(hooks?: Hooks, files: Record<string, string> = {}) {
  const { filesystem, stats } = observed(memoryFilesystem(files).open("/data", true), hooks);
  const standard = new StandardHost(ALL, [{ path: "/data", writable: true, filesystem }], 8 << 20);
  const [[root]] = result("getDirectories", standard.sync(call("getDirectories")));
  const opened = result(
    "openAt",
    (
      await standard.async(
        call("openAt", root, {}, "out.bin", { create: true }, { read: true, write: true }),
      )
    ).bytes,
  );
  expect(opened.tag).toBe("ok");
  return { standard, stats, root: root as number, file: opened.val as number };
}

test("one writeStream call receives every chunk of a stream at the exact offset", async () => {
  const { standard, stats, file } = await setup();
  const bytes = Uint8Array.from({ length: 200_003 }, (_, at) => (at * 31 + 0x80) % 256);
  const writer = writerOf(standard);
  const future = result("writeViaStream", standard.sync(call("writeViaStream", file, writer, 5n)));
  for (let at = 0; at < bytes.length; at += 50_000)
    await standard.async(write(writer, bytes.subarray(at, at + 50_000)));
  standard.sync(packet(DROP, writer));
  expect(await settle(standard, future)).toEqual({ tag: "ok" });
  expect(stats.streams).toEqual([5n]);
  expect(stats.writes).toBe(0);
  expect(stats.sizes.length).toBeGreaterThan(1);
  expect(Math.max(...stats.sizes)).toBeLessThanOrEqual(CHUNK);
  expect(stats.sizes.reduce((sum, size) => sum + size, 0)).toBe(bytes.length);
  // Append is a stream without an offset: it starts at the end of file.
  const appender = writerOf(standard);
  const appended = result(
    "appendViaStream",
    standard.sync(call("appendViaStream", file, appender)),
  );
  await standard.async(write(appender, Uint8Array.of(0xfe, 0xff)));
  standard.sync(packet(DROP, appender));
  expect(await settle(standard, appended)).toEqual({ tag: "ok" });
  expect(stats.streams).toEqual([5n, undefined]);
  const read = await readAll(standard, file);
  expect(read.subarray(0, 5)).toEqual(new Uint8Array(5));
  expect(read.subarray(5, 200_008)).toEqual(bytes);
  expect(read.subarray(200_008)).toEqual(Uint8Array.of(0xfe, 0xff));
  expect(standard.inputBytes).toBe(0);
  await standard.dispose();
});

test("providers without writeStream fall back to one write per chunk", async () => {
  const { standard, stats, file } = await setup({ stream: false });
  const writer = writerOf(standard);
  const future = result("writeViaStream", standard.sync(call("writeViaStream", file, writer, 0n)));
  for (let at = 0; at < 3; at++) {
    await standard.async(write(writer, new Uint8Array(CHUNK).fill(at + 1)));
    // Let the consumer take each chunk so every provider write sees one chunk.
    await tick();
  }
  standard.sync(packet(DROP, writer));
  expect(await settle(standard, future)).toEqual({ tag: "ok" });
  expect([stats.streams.length, stats.writes]).toEqual([0, 3]);
  expect((await readAll(standard, file)).length).toBe(3 * CHUNK);
  await standard.dispose();
});

test("a provider stream failure stops the input, reports its code and frees the budget", async () => {
  const { standard, file } = await setup({
    chunk(index) {
      if (index === 1) throw new FilesystemError("insufficient-space");
    },
  });
  const writer = writerOf(standard);
  const future = result("writeViaStream", standard.sync(call("writeViaStream", file, writer, 0n)));
  await standard.async(write(writer, new Uint8Array(1000).fill(1)));
  await tick();
  await standard.async(write(writer, new Uint8Array(1000).fill(2)));
  expect(await settle(standard, future)).toEqual({
    tag: "err",
    val: { tag: "insufficient-space" },
  });
  expect((await standard.async(write(writer, Uint8Array.of(3)))).bytes).toEqual(
    Uint8Array.of(WRITE_CLOSED),
  );
  expect(standard.inputBytes).toBe(0);
  // Accepted bytes before the failure stay written.
  expect((await readAll(standard, file)).length).toBe(1000);
  standard.sync(packet(DROP, writer));
  await standard.dispose();
});

test("callback end keeps a provider stream physically owned and the file open until it settles", async () => {
  const gate = deferred();
  const entered = deferred();
  const { standard, stats, file } = await setup({
    async chunk(index) {
      if (index === 1) {
        entered.resolve();
        await gate.promise;
      }
    },
  });
  const writer = writerOf(standard);
  const future = result("writeViaStream", standard.sync(call("writeViaStream", file, writer, 0n)));
  await standard.async(write(writer, new Uint8Array(10).fill(1)));
  await tick();
  await standard.async(write(writer, new Uint8Array(20).fill(2)));
  await entered.promise;
  // The guest drops its descriptor; the stream still owns the provider file.
  standard.sync(packet(DROP, file));
  expect(stats.closes).toBe(0);
  standard.endCallback();
  let settled = false;
  const physical = standard.settled().then(() => {
    settled = true;
  });
  await tick();
  expect(settled).toBe(false);
  expect(stats.closes).toBe(0);
  gate.resolve();
  await physical;
  expect(stats.closes).toBe(1);
  expect(await settle(standard, future)).toEqual({ tag: "ok" });
  await standard.dispose();
});

test("all writers share a 1 MiB accepted-input budget, reserved before any copy", async () => {
  const { standard, file } = await setup();
  const full = new Uint8Array(CHUNK);
  const writers = Array.from({ length: INPUT_BUDGET / CHUNK }, () => writerOf(standard));
  // Untransferred writers have no consumer, so each holds its chunk.
  for (const writer of writers) await standard.async(write(writer, full));
  expect(standard.inputBytes).toBe(INPUT_BUDGET);
  const late = writerOf(standard);
  let accepted = false;
  const waiting = standard.async(write(late, full)).then(() => {
    accepted = true;
  });
  await tick();
  expect(accepted).toBe(false);
  // Refusals happen before reserving or copying: the budget does not move.
  const oversized = packet(STREAM_WRITE, late, (w) => w.bytes(new Uint8Array(CHUNK + 1)));
  await expect(standard.async(oversized)).rejects.toThrow("64 KiB");
  const truncated = packet(STREAM_WRITE, late, (w) => w.u32(CHUNK));
  await expect(standard.async(truncated)).rejects.toThrow("truncated");
  await expect(standard.async(write(late, full))).rejects.toThrow("already pending");
  await expect(standard.async(write(0x7fff_ff00, full))).rejects.toThrow("not a live input");
  const other = writerOf(standard);
  const controller = new AbortController();
  const aborted = standard.async(write(other, full), controller.signal);
  controller.abort();
  await expect(aborted).rejects.toThrow("cancelled");
  expect(standard.inputBytes).toBe(INPUT_BUDGET);
  expect(accepted).toBe(false);
  // Dropping an untransferred writer discards its bytes and admits the waiting chunk.
  standard.sync(packet(DROP, writers[0]!));
  await waiting;
  expect(standard.inputBytes).toBe(INPUT_BUDGET);
  // A consumer frees its stream's reservation as it takes the bytes.
  const future = result(
    "writeViaStream",
    standard.sync(call("writeViaStream", file, writers[1]!, 0n)),
  );
  standard.sync(packet(DROP, writers[1]!));
  expect(await settle(standard, future)).toEqual({ tag: "ok" });
  expect(standard.inputBytes).toBe(INPUT_BUDGET - CHUNK);
  // Callback end closes the rest: untransferred bytes are discarded.
  standard.endCallback();
  expect(standard.inputBytes).toBe(0);
  await standard.dispose();
});

test("a writer dropped while waiting for budget fails its write and reserves nothing", async () => {
  const { standard } = await setup();
  const full = new Uint8Array(CHUNK);
  const writers = Array.from({ length: INPUT_BUDGET / CHUNK }, () => writerOf(standard));
  for (const writer of writers) await standard.async(write(writer, full));
  const late = writerOf(standard);
  const waiting = standard.async(write(late, full));
  await tick();
  standard.sync(packet(DROP, late));
  await expect(waiting).rejects.toThrow("stream was dropped");
  expect(standard.inputBytes).toBe(INPUT_BUDGET);
  standard.endCallback();
  expect(standard.inputBytes).toBe(0);
  await standard.dispose();
});

test("asynchronous provider close and iterator return stay physically owned", async () => {
  const closing = deferred();
  const returning = deferred();
  const { standard, stats, root } = await setup(
    { close: () => closing.promise, returned: () => returning.promise },
    { a: "1", b: "2" },
  );
  const [stream] = result("readDirectory", standard.sync(call("readDirectory", root)));
  await standard.async(packet(STREAM_READ, stream, (w) => w.u32(1)));
  standard.sync(packet(DROP, stream));
  let settled = false;
  const physical = standard.settled().then(() => {
    settled = true;
  });
  await tick();
  expect(settled).toBe(false);
  returning.resolve();
  await physical;
  let disposed = false;
  const disposal = standard.dispose().then(() => {
    disposed = true;
  });
  await tick();
  expect(stats.closes).toBe(2);
  expect(disposed).toBe(false);
  closing.resolve();
  await disposal;
});
