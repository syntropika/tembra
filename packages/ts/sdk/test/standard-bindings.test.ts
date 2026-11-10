// The real guest standard bindings (bundled as the interpreter loads them) connected to a real
// browser StandardHost over the memory provider, without the Wasm interpreter in between.
import { expect, test } from "bun:test";
import { runInNewContext } from "node:vm";
import path from "node:path";
import {
  type Descriptor,
  type DirectoryDescriptor,
  memoryFilesystem,
  type MountedFilesystem,
} from "../src/interpreter/filesystem.ts";
import { CALL, Writer, STREAM_WRITE } from "../src/interpreter/standard-codec.ts";
import { METHODS } from "../src/interpreter/standard.ts";
import { StandardHost } from "../src/interpreter/standard-host.ts";
import {
  FilesystemCallError,
  readBytes,
  readChunks,
  readText,
  writeBytes,
  writeChunks,
  writeText,
} from "../src/guest.ts";

const FS = "wasi:filesystem/types@0.3.0";
const PREOPENS = "wasi:filesystem/preopens@0.3.0";
const built = await Bun.build({
  entrypoints: [
    path.resolve(import.meta.dir, "../../../../crates/executor-wasm/guest/src/standard.ts"),
  ],
  target: "node",
  format: "iife",
});
if (!built.success) throw new AggregateError(built.logs);
const script = await built.outputs[0]!.text();
const tick = () => new Promise((resolve) => setTimeout(resolve, 0));

/** Hold provider file writes until released, by wrapping descriptors (providers freeze theirs). */
function gated(base: MountedFilesystem) {
  let release: (() => void) | undefined;
  let blocked: Promise<void> | undefined;
  const control = {
    hold() {
      blocked = new Promise((resolve) => {
        release = resolve;
      });
    },
    release() {
      release?.();
      blocked = undefined;
    },
  };
  const wrap = (descriptor: Descriptor): Descriptor => {
    const wrapper: Record<string, unknown> = {};
    for (const key of [
      "type",
      "writable",
      "stat",
      "isSameObject",
      "sync",
      "close",
      "read",
      "append",
      "setSize",
      "openAt",
      "statAt",
      "readDirectory",
      "createDirectoryAt",
      "removeDirectoryAt",
      "unlinkFileAt",
      "renameAt",
    ]) {
      const value = (descriptor as unknown as Record<string, unknown>)[key];
      if (value !== undefined)
        wrapper[key] = typeof value === "function" ? value.bind(descriptor) : value;
    }
    if (descriptor.type === "directory")
      wrapper.openAt = async (...args: Parameters<typeof descriptor.openAt>) =>
        wrap(await descriptor.openAt(...args));
    else
      wrapper.write = async (offset: bigint, data: Uint8Array) => {
        await blocked;
        return descriptor.write(offset, data);
      };
    return wrapper as unknown as Descriptor;
  };
  const filesystem: MountedFilesystem = {
    writable: base.writable,
    openRoot: () => wrap(base.openRoot()) as DirectoryDescriptor,
    dispose: () => base.dispose(),
  };
  return { filesystem, control };
}

/** Load the guest bootstrap against a fresh host. */
function guest(
  filesystem: MountedFilesystem = memoryFilesystem().open("/data", true),
  extraImports: { contract: string; type_only: boolean }[] = [],
) {
  const imports = [
    { contract: FS, type_only: false },
    { contract: PREOPENS, type_only: false },
    ...extraImports,
  ];
  const standard = new StandardHost(
    new Set(imports.filter((item) => !item.type_only).map((item) => item.contract)),
    [{ path: "/data", writable: filesystem.writable, filesystem }],
    8 << 20,
  );
  const writes: number[] = [];
  const globals: any = {
    Uint8Array,
    DataView,
    Symbol,
    __tembraContract: { imports },
    __hostSyncBytes(packet: Uint8Array) {
      try {
        return standard.sync(packet);
      } catch (error) {
        // The port reports synchronous refusals as bare strings.
        throw error instanceof Error ? error.message : String(error);
      }
    },
    __hostAsync(packet: Uint8Array) {
      if (packet[1] === STREAM_WRITE) writes.push(packet.length - 14);
      return standard.async(packet).then((delivery) => delivery.bytes);
    },
  };
  runInNewContext(script, globals);
  const [[root]] = globals.__tembraStandard[PREOPENS].getDirectories();
  return {
    standard,
    writes,
    root,
    modules: globals.__tembraStandard,
    /** The runtime's callback-end boundary. */
    endCallback: () => globals.__tembraSweepStandard(true),
  };
}

test("text and byte helpers round-trip through standard descriptors in bounded chunks", async () => {
  const { root, writes, standard } = guest();
  const file = await root.openAt({}, "notes.txt", { create: true }, { read: true, write: true });
  await writeText(file, "héllo € \u{1f600}");
  expect(await readText(file)).toBe("héllo € \u{1f600}");
  const binary = Uint8Array.from({ length: 200_000 }, (_, at) => (at * 13 + 0xc0) % 256);
  await writeBytes(file, binary, { offset: 10n });
  expect(await readBytes(file, { offset: 10n })).toEqual(binary);
  expect(Math.max(...writes)).toBe(64 << 10);
  await writeBytes(file, Uint8Array.of(0xff), { append: true });
  expect((await file.stat()).size).toBe(200_011n);
  await expect(readBytes(file, { maxBytes: 100 })).rejects.toThrow(RangeError);
  await expect(readText(file)).rejects.toThrow("invalid UTF-8");
  file[Symbol.dispose]();
  root[Symbol.dispose]();
  expect(standard.size).toBe(0);
  await standard.dispose();
});

test("plain AsyncIterable<number> inputs are batched into 64 KiB writes and finish", async () => {
  const { root, writes, standard } = guest();
  const file = await root.openAt({}, "b.bin", { create: true }, { read: true, write: true });
  let finished = false;
  async function* bytes() {
    try {
      for (let at = 0; at < 70_000; at++) yield at % 251;
    } finally {
      finished = true;
    }
  }
  expect(await file.writeViaStream(bytes(), 0n)).toEqual({ tag: "ok" });
  expect(finished).toBe(true);
  expect(writes).toEqual([65_536, 70_000 - 65_536]);
  const read = await readBytes(file);
  expect(read.length).toBe(70_000);
  expect(read[69_999]).toBe(69_999 % 251);
  await standard.dispose();
});

test("typed directory streams iterate entries and report completion", async () => {
  const { root, standard } = guest(
    memoryFilesystem({ "a.txt": "a", "dir/inner": "i" }).open("/data", true),
  );
  await root.createDirectoryAt("made");
  const [entries, done] = root.readDirectory();
  const seen: string[] = [];
  for await (const entry of entries) seen.push(`${entry.name}:${entry.type.tag}`);
  expect(seen.sort()).toEqual(["a.txt:regular-file", "dir:directory", "made:directory"]);
  expect(await done).toEqual({ tag: "ok" });
  entries[Symbol.dispose]();
  await expect(root.removeDirectoryAt("dir")).rejects.toThrow("not-empty");
  await standard.dispose();
});

test("a stopped consumer returns the source and the helper reports the WASI error", async () => {
  const { root, standard } = guest(memoryFilesystem({ "r.txt": "r" }).open("/data", true));
  const file = await root.openAt({}, "r.txt", {}, { read: true });
  let returned = false;
  function* chunks() {
    try {
      for (;;) yield new Uint8Array(1024);
    } finally {
      returned = true;
    }
  }
  const failure = await writeBytes(file, new Uint8Array(10)).catch((error) => error);
  expect(failure).toBeInstanceOf(FilesystemCallError);
  expect(failure.code).toBe("bad-descriptor");
  expect(await file.writeViaStream(writeChunks(chunks()), 0n)).toEqual({
    tag: "err",
    val: { tag: "bad-descriptor" },
  });
  await tick();
  expect(returned).toBe(true);
  await standard.dispose();
});

test("callback end returns unfinished sources, drops writers and keeps accepted bytes", async () => {
  const { filesystem, control } = gated(memoryFilesystem().open("/data", true));
  const { root, standard, endCallback } = guest(filesystem);
  const file = await root.openAt({}, "slow.bin", { create: true }, { read: true, write: true });
  control.hold();
  let produced = 0;
  let returned = false;
  async function* forever() {
    try {
      for (;;) {
        produced++;
        yield new Uint8Array(64 << 10).fill(produced);
      }
    } finally {
      returned = true;
    }
  }
  const future = file.writeViaStream(writeChunks(forever()), 0n);
  // One chunk is in the gated provider write, one is buffered, one write is pending.
  for (let wait = 0; wait < 20 && produced < 3; wait++) await tick();
  expect(produced).toBe(3);
  const live = standard.size;
  endCallback();
  // Async generator cleanup runs in a later job.
  await tick();
  expect(returned).toBe(true);
  expect(standard.size).toBe(live - 1);
  control.release();
  expect(await future).toEqual({ tag: "ok" });
  await standard.settled();
  expect((await file.stat()).size).toBe(2n * BigInt(64 << 10));
  // Keep `root` reachable: a collected root is legitimately dropped by the sweep above.
  root[Symbol.dispose]();
  await standard.dispose();
});

test("a source failure rejects the future after accepted bytes are written", async () => {
  const { root, standard } = guest();
  const file = await root.openAt({}, "partial.bin", { create: true }, { read: true, write: true });
  async function* broken() {
    yield new Uint8Array(5).fill(1);
    throw new Error("source broke");
  }
  const future = file.writeViaStream(writeChunks(broken()), 0n);
  await expect(future.catch((error: Error) => Promise.reject(error))).rejects.toThrow(
    "source broke",
  );
  expect((await file.stat()).size).toBe(5n);
  await standard.dispose();
});

test("a refused call discards its untransferred writer and returns the source", async () => {
  const { root, standard } = guest();
  const file = await root.openAt({}, "x.bin", { create: true }, { read: true, write: true });
  const live = standard.size;
  let returned = false;
  const source = {
    [Symbol.asyncIterator]() {
      return {
        next: async () => ({ done: false, value: 1 }),
        return: async () => {
          returned = true;
          return { done: true, value: undefined };
        },
      };
    },
  };
  // The offset is encoded after the stream: the writer exists, then encoding fails.
  expect(() => file.writeViaStream(source, -1n)).toThrow("64-bit integer -1");
  expect(returned).toBe(true);
  expect(standard.size).toBe(live);
  expect(() => file.writeViaStream({}, 0n)).toThrow("AsyncIterable");
  expect(standard.size).toBe(live);
  root[Symbol.dispose]();
  await standard.dispose();
});

test("byte and directory stream iteration releases handles at EOF and early return", async () => {
  const { root, standard } = guest(
    memoryFilesystem({ "data.bin": new Uint8Array(150_000) }).open("/data", false),
  );
  const file = await root.openAt({}, "data.bin", {}, { read: true });
  const baseline = standard.size;
  for (const early of [false, true]) {
    for (const chunked of [false, true]) {
      const [stream, done] = file.readViaStream(0n);
      for await (const chunk of chunked ? readChunks(stream) : stream) {
        if (!(chunked ? chunk instanceof Uint8Array : typeof chunk === "number"))
          throw new Error("Stream returned a value with the wrong item type");
        if (early) break;
      }
      await done;
      expect(standard.size).toBe(baseline);
      stream[Symbol.dispose]();
      expect(standard.size).toBe(baseline);
    }
    const [stream, done] = root.readDirectory();
    for await (const entry of stream) {
      expect(entry.name).toBe("data.bin");
      if (early) break;
    }
    await done;
    expect(standard.size).toBe(baseline);
  }
  file[Symbol.dispose]();
  root[Symbol.dispose]();
  expect(standard.size).toBe(0);
  await standard.dispose();
});

test("a file-copy pump drops its byte-stream source when the destination stops early", async () => {
  const { root, standard } = guest(
    memoryFilesystem({ "source.bin": new Uint8Array(150_000) }).open("/data", true),
  );
  const source = await root.openAt({}, "source.bin", {}, { read: true });
  const destination = await root.openAt({}, "destination.bin", { create: true }, { read: true });
  const baseline = standard.size;
  const [stream, readDone] = source.readViaStream(0n);
  const writeDone = destination.writeViaStream(stream, 0n);
  expect((await writeDone).tag).toBe("err");
  await readDone;
  expect(standard.size).toBe(baseline);
  destination[Symbol.dispose]();
  source[Symbol.dispose]();
  root[Symbol.dispose]();
  await standard.dispose();
});

test("type-only standard dependencies expose types without callable authority", async () => {
  const clock = "wasi:clocks/monotonic-clock@0.3.0";
  const hidden = guest(undefined, [{ contract: clock, type_only: true }]);
  expect(hidden.modules[clock]).toBeUndefined();
  const writer = new Writer();
  writer.u8(0);
  writer.u8(CALL);
  writer.u32(METHODS.findIndex((method) => method.interface === clock && method.name === "now"));
  expect(() => hidden.standard.sync(writer.finish())).toThrow("not imported");
  hidden.root[Symbol.dispose]();
  await hidden.standard.dispose();
  const explicit = guest(undefined, [{ contract: clock, type_only: false }]);
  expect(typeof explicit.modules[clock].now()).toBe("bigint");
  explicit.root[Symbol.dispose]();
  await explicit.standard.dispose();
});
