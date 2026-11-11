import { expect, test } from "bun:test";
import { CoreModules, type InstrumentedCore } from "../src/interpreter/component/compiler.ts";

/** A real one-page memory module with a large custom section, like a shared guest runtime. */
function moduleBytes(marker = 0): Uint8Array {
  const prefix = Uint8Array.of(
    0,
    97,
    115,
    109,
    1,
    0,
    0,
    0,
    5,
    3,
    1,
    0,
    1,
    7,
    10,
    1,
    6,
    109,
    101,
    109,
    111,
    114,
    121,
    2,
    0,
    0,
    129,
    128,
    4,
    0,
  );
  const bytes = new Uint8Array(prefix.length + 65536);
  bytes.set(prefix);
  bytes[bytes.length - 1] = marker;
  return bytes;
}

function instrumenter() {
  let calls = 0;
  let frees = 0;
  return {
    get calls() {
      return calls;
    },
    get frees() {
      return frees;
    },
    instrument(bytes: Uint8Array, _maximumPages: number): InstrumentedCore {
      calls++;
      return {
        bytes: () => bytes,
        initialPages: () => 1n,
        free: () => {
          frees++;
        },
      };
    },
  };
}

test("identical runtime bytes share code across components and retain independent memories", async () => {
  const cache = new CoreModules();
  const policy = instrumenter();
  const [first, second] = await Promise.all([
    cache.compile(moduleBytes(), 16, policy.instrument),
    cache.compile(moduleBytes(), 16, policy.instrument),
  ]);
  expect(first.module).toBe(second.module);
  expect(policy.calls).toBe(1);
  expect(policy.frees).toBe(1);
  const a = await WebAssembly.instantiate(first.module);
  const b = await WebAssembly.instantiate(second.module);
  const left = new Uint8Array((a.exports.memory as WebAssembly.Memory).buffer);
  const right = new Uint8Array((b.exports.memory as WebAssembly.Memory).buffer);
  left[0] = 42;
  expect(right[0]).toBe(0);
  expect(first.pages).toBe(1n);
});

test("different bytes, memory limits and verifier policies cannot reuse one another's code", async () => {
  const cache = new CoreModules();
  const policy = instrumenter();
  const other = instrumenter();
  const a = await cache.compile(moduleBytes(), 16, policy.instrument);
  const b = await cache.compile(moduleBytes(1), 16, policy.instrument);
  const c = await cache.compile(moduleBytes(), 32, policy.instrument);
  const d = await cache.compile(moduleBytes(), 16, other.instrument);
  expect(new Set([a.module, b.module, c.module, d.module]).size).toBe(4);
  expect(policy.calls).toBe(3);
  expect(other.calls).toBe(1);
});

test("failed compilation releases the instrumenter and does not poison retries", async () => {
  const cache = new CoreModules();
  let attempts = 0;
  let freed = 0;
  const instrument = (bytes: Uint8Array) => ({
    bytes: () => (++attempts === 1 ? Uint8Array.of(1) : bytes),
    initialPages: () => 1n,
    free: () => {
      freed++;
    },
  });
  await expect(cache.compile(moduleBytes(), 16, instrument)).rejects.toBeInstanceOf(
    WebAssembly.CompileError,
  );
  const recovered = await cache.compile(moduleBytes(), 16, instrument);
  expect(recovered.module).toBeInstanceOf(WebAssembly.Module);
  expect(attempts).toBe(2);
  expect(freed).toBe(2);
});

test("bytes are captured before asynchronous hashing so caller mutation cannot poison the cache", async () => {
  const cache = new CoreModules();
  const policy = instrumenter();
  const bytes = moduleBytes();
  const original = cache.compile(bytes, 16, policy.instrument);
  bytes[bytes.length - 1] = 1;
  const changed = cache.compile(bytes, 16, policy.instrument);
  const [a, b] = await Promise.all([original, changed]);
  expect(a.module).not.toBe(b.module);
  expect(policy.calls).toBe(2);
  expect((await cache.compile(moduleBytes(), 16, policy.instrument)).module).toBe(a.module);
});

test("execution remains available without WebCrypto while code caching is disabled", async () => {
  const original = Object.getOwnPropertyDescriptor(globalThis, "crypto");
  Object.defineProperty(globalThis, "crypto", { value: undefined, configurable: true });
  try {
    const cache = new CoreModules();
    const policy = instrumenter();
    const first = await cache.compile(moduleBytes(), 16, policy.instrument);
    const second = await cache.compile(moduleBytes(), 16, policy.instrument);
    expect(policy.calls).toBe(2);
    expect(policy.frees).toBe(2);
    const left = await WebAssembly.instantiate(first.module);
    const right = await WebAssembly.instantiate(second.module);
    const a = new Uint8Array((left.exports.memory as WebAssembly.Memory).buffer);
    const b = new Uint8Array((right.exports.memory as WebAssembly.Memory).buffer);
    a[0] = 42;
    expect(b[0]).toBe(0);
  } finally {
    if (original) Object.defineProperty(globalThis, "crypto", original);
    else Reflect.deleteProperty(globalThis, "crypto");
  }
});
