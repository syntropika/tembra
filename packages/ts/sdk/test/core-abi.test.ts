import { expect, test } from "bun:test";
import path from "node:path";
import * as abi from "../src/interpreter/abi.ts";
import { checkInterpreter, CoreGuest, wasi, type GuestHost } from "../src/interpreter/core.ts";
import { query, type ContractPort } from "../src/interpreter/types.ts";

// Actual-engine transport checks; set TEMBRA_WASM_ENGINE and TEMBRA_WASM_FIXTURES to run them.
const engine = process.env.TEMBRA_WASM_ENGINE;
const fixtures = process.env.TEMBRA_WASM_FIXTURES;
const actual = engine && fixtures ? test : test.skip;

/** Assemble a module importing one function `() -> ()`, or a memory, under the given names. */
function importing(module: string, name: string, memory = false): Uint8Array<ArrayBuffer> {
  const text = (value: string) => [value.length, ...new TextEncoder().encode(value)];
  const entry = [...text(module), ...text(name), ...(memory ? [2, 0, 1] : [0, 0])];
  const sections = memory ? [] : [1, 4, 1, 0x60, 0, 0];
  return new Uint8Array([
    ...[0, 0x61, 0x73, 0x6d, 1, 0, 0, 0],
    ...sections,
    ...[2, entry.length + 1, 1, ...entry],
  ]);
}

/** Load the source fixture and its prepared contract description through the contract module. */
async function fixture(): Promise<{ source: string; contract: unknown }> {
  const contracts = await import("@syntropika/tembra-engine-wasm/contracts");
  await contracts.default({
    module_or_path: await Bun.file(
      Bun.fileURLToPath(import.meta.resolve("@syntropika/tembra-engine-wasm/contracts_bg.wasm")),
    ).arrayBuffer(),
  });
  const bytes = await Bun.file(path.join(fixtures!, "arithmetic-source.wasm")).bytes();
  const artifact = new contracts.WasmArtifact(bytes) as unknown as ContractPort & {
    source(): string;
  };
  return { source: artifact.source(), contract: query(artifact, { kind: "describe" }) };
}

const compiled = async () => {
  const module = await WebAssembly.compile(await Bun.file(engine!).arrayBuffer());
  checkInterpreter(module);
  return module;
};

test("only the exact private imports are accepted", async () => {
  for (const bytes of [
    importing(abi.WASI_MODULE, "sock_accept"),
    importing(abi.WASI_MODULE, "random_get"),
    importing(abi.IMPORT_MODULE, "spawn"),
    importing("tembra-interpreter-v1", abi.SYNC_IMPORT),
    importing("env", "table_memory", true),
  ])
    expect(() => checkInterpreter(new WebAssembly.Module(bytes))).toThrow("is not allowed");
  expect(() =>
    checkInterpreter(new WebAssembly.Module(importing(abi.WASI_MODULE, "fd_write"))),
  ).toThrow("is missing");
});

test("standard descriptors and clocks behave without capabilities", () => {
  const memory = new WebAssembly.Memory({ initial: 1, maximum: 1 });
  const logs: string[] = [];
  const functions = wasi(memory, {
    sync: () => "null",
    log: (level, text) => logs.push(`${level}:${text}`),
  });
  const view = new DataView(memory.buffer);
  expect(functions.clock_time_get!(abi.CLOCK_REALTIME, 0n, 0)).toBe(abi.ERRNO_SUCCESS);
  expect(view.getBigUint64(0, true)).toBe(0n);
  expect(functions.clock_time_get!(2, 0n, 0)).toBe(abi.ERRNO_INVAL);
  expect(functions.clock_time_get!(abi.CLOCK_REALTIME, 0n, 65532)).toBe(abi.ERRNO_FAULT);
  expect(functions.fd_prestat_get!(3, 0)).toBe(abi.ERRNO_BADF);
  expect(functions.path_open!(3)).toBe(abi.ERRNO_BADF);
  expect(functions.path_open!(1)).toBe(abi.ERRNO_NOTDIR);
  expect(functions.fd_seek!(1, 0n, 0, 0)).toBe(abi.ERRNO_SPIPE);
  new Uint8Array(memory.buffer).set(new TextEncoder().encode("abcde\n"), 96);
  view.setUint32(64, 96, true);
  view.setUint32(68, 6, true);
  expect(functions.fd_write!(2, 64, 1, 80)).toBe(abi.ERRNO_SUCCESS);
  expect(view.getUint32(80, true)).toBe(6);
  expect(functions.fd_write!(0, 64, 1, 80)).toBe(abi.ERRNO_BADF);
  expect(functions.fd_close!(2)).toBe(abi.ERRNO_SUCCESS);
  expect(functions.fd_write!(2, 64, 1, 80)).toBe(abi.ERRNO_BADF);
  expect(logs).toEqual(["error:abcde"]);
  expect(() => functions.proc_exit!(3)).toThrow("status 3");
});

test("interpreter libc reads only its supplied host clock", () => {
  const memory = new WebAssembly.Memory({ initial: 1, maximum: 1 });
  const calls: number[] = [];
  const functions = wasi(memory, {
    sync: () => "null",
    log() {},
    clock(clock) {
      calls.push(clock);
      return clock === abi.CLOCK_REALTIME ? 1_234_567_890_000_000_123n : 9_007_199_254_740_993n;
    },
  });
  const view = new DataView(memory.buffer);
  functions.clock_time_get!(abi.CLOCK_REALTIME, 0n, 0);
  expect(view.getBigUint64(0, true)).toBe(1_234_567_890_000_000_123n);
  functions.clock_time_get!(abi.CLOCK_MONOTONIC, 0n, 0);
  expect(view.getBigUint64(0, true)).toBe(9_007_199_254_740_993n);
  expect(functions.clock_time_get!(2, 0n, 0)).toBe(abi.ERRNO_INVAL);
  expect(functions.clock_time_get!(abi.CLOCK_REALTIME, 0n, 65532)).toBe(abi.ERRNO_FAULT);
  expect(calls).toEqual([abi.CLOCK_REALTIME, abi.CLOCK_MONOTONIC]);
});

actual("the core transport drives effects, capture and isolated bounded memory", async () => {
  const module = await compiled();
  const { source, contract } = await fixture();
  const packets: string[] = [];
  const host: GuestHost = {
    sync(packet) {
      packets.push(packet as string);
      return "null";
    },
    log() {},
  };
  const memoryBytes = 32 << 20;
  const guest = await CoreGuest.create(module, memoryBytes, host);
  const loaded = guest.load(JSON.stringify({ source, heap_bytes: 8 << 20, contract }));
  expect(loaded.ok && !loaded.interrupted).toBe(true);
  expect(JSON.parse(loaded.value).services.arithmetic.methods).toContain("double");
  const invoke = (value: number) =>
    JSON.stringify({
      kind: "invoke",
      environment: 1,
      service: "arithmetic",
      method: "double",
      args: [value],
    });
  expect(guest.start(invoke(3)).outcome).toEqual({ ok: true, value: "6", interrupted: false });
  // A pending effect carries a JavaScript-number identity and blocks capture until settled.
  const pending = guest.start(invoke(6));
  expect(pending.outcome).toBeUndefined();
  expect(pending.effects).toHaveLength(1);
  const [effect] = pending.effects;
  expect(typeof effect!.id).toBe("number");
  expect(JSON.parse(effect!.packet as string)).toEqual({ operation: "wait", milliseconds: 200 });
  expect(() => guest.capture()).toThrow("closed admission");
  expect(guest.trapped).toBe(false);
  const done = guest.resume([{ id: effect!.id, ok: true, value: "null" }]);
  expect(done.outcome).toEqual({ ok: true, value: "12", interrupted: false });
  expect(packets.some((packet) => packet.includes("slow-finished"))).toBe(true);
  expect(guest.capture().byteLength).toBeGreaterThan(1024);
  // The VM heap limit exceeds the linear-memory maximum; only that instance fails.
  const bounded = await CoreGuest.create(module, 4 << 20, host);
  let failed = false;
  try {
    const outcome = bounded.load(
      JSON.stringify({
        source: `const chunks=[];for(let i=0;i<64;i++)chunks.push(new Uint8Array(1<<20));${source}`,
        heap_bytes: 256 << 20,
        contract,
      }),
    );
    failed = !outcome.ok && outcome.interrupted;
  } catch {
    failed = bounded.trapped;
  }
  expect(failed).toBe(true);
  expect(bounded.memory.buffer.byteLength).toBeLessThanOrEqual(4 << 20);
  expect(guest.start(invoke(4)).outcome?.value).toBe("8");
  expect(guest.memory.buffer.byteLength).toBeLessThanOrEqual(memoryBytes);
});

actual("raw ABI misuse is refused and reported as interruption", async () => {
  const module = await compiled();
  const { source, contract } = await fixture();
  const memory = new WebAssembly.Memory({ initial: abi.MEMORY_INITIAL_PAGES, maximum: 512 });
  let reply: Uint8Array | undefined;
  const host: GuestHost = { sync: () => "null", log() {} };
  const instance = await WebAssembly.instantiate(module, {
    [abi.MEMORY_MODULE]: { [abi.MEMORY_IMPORT]: memory },
    [abi.IMPORT_MODULE]: {
      [abi.SYNC_IMPORT]: () => {
        reply = new TextEncoder().encode("null");
        return reply.byteLength;
      },
      [abi.SYNC_READ_IMPORT]: (pointer: number) => {
        new Uint8Array(memory.buffer, pointer, reply!.byteLength).set(reply!);
      },
    },
    [abi.WASI_MODULE]: wasi(memory, host),
  });
  const exports = instance.exports as Record<string, (...args: number[]) => number>;
  exports[abi.INITIALIZE_EXPORT]!();
  const alloc = exports[abi.ALLOC_EXPORT]!,
    call = exports[abi.CALL_EXPORT]!;
  /** Decode the header fields of a reply from the current buffer. */
  const header = (pointer: number) => {
    const view = new DataView(memory.buffer, pointer >>> 0);
    return { kind: view.getUint32(4, true), interrupted: view.getUint32(8, true) };
  };
  /** Copy text into a fresh reservation. */
  const input = (bytes: Uint8Array): number => {
    const pointer = alloc(bytes.byteLength) >>> 0;
    new Uint8Array(memory.buffer, pointer, bytes.byteLength).set(bytes);
    return pointer;
  };
  const load = new TextEncoder().encode(JSON.stringify({ source, heap_bytes: 8 << 20, contract }));
  expect(header(call(abi.Operation.Load, input(load), load.byteLength))).toEqual({
    kind: abi.ReplyKind.Ok,
    interrupted: 0,
  });
  // A second reservation without a call releases both and is refused.
  expect(alloc(4)).not.toBe(0);
  expect(alloc(4)).toBe(0);
  // Unknown operations, unreserved inputs and stray buffers are refused as interruptions.
  for (const [operation, pointer, length] of [
    [99, 0, 0],
    [abi.Operation.Start, 64, 2],
    [abi.Operation.Capture, 64, 2],
  ] as const)
    expect(header(call(operation, pointer, length))).toEqual({
      kind: abi.ReplyKind.Error,
      interrupted: 1,
    });
  // Invalid UTF-8 never reaches the engine.
  const invalid = new Uint8Array([0xff, 0xfe]);
  expect(header(call(abi.Operation.Declare, input(invalid), 2))).toEqual({
    kind: abi.ReplyKind.Error,
    interrupted: 1,
  });
});

test("WASIp1 clock conversion saturates instead of wrapping", () => {
  const memory = new WebAssembly.Memory({ initial: 1, maximum: 1 });
  let nanoseconds = -1n;
  const functions = wasi(memory, { sync: () => "null", log() {}, clock: () => nanoseconds });
  const view = new DataView(memory.buffer);
  functions.clock_time_get!(abi.CLOCK_REALTIME, 0n, 0);
  expect(view.getBigUint64(0, true)).toBe(0n);
  nanoseconds = 1n << 64n;
  functions.clock_time_get!(abi.CLOCK_REALTIME, 0n, 0);
  expect(view.getBigUint64(0, true)).toBe((1n << 64n) - 1n);
});
