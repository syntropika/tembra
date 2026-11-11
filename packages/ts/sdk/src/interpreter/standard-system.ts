/** Browser providers for the standard clock and random interfaces. */
import { abortable, cancelled, type Implementation } from "./standard-streams.ts";

/** Signed seconds since the epoch plus a nonnegative fractional nanosecond part. */
export interface ClockInstant {
  readonly seconds: bigint;
  readonly nanoseconds: number;
}

/** Host clocks shared by WASI functions and the source VM's time imports. */
export interface ClockProvider {
  systemNow(): ClockInstant;
  systemResolution(): bigint;
  monotonicNow(): bigint;
  monotonicResolution(): bigint;
  waitUntil(when: bigint, signal?: AbortSignal): Promise<void>;
}

/** Secure bytes must come from a cryptographic source; insecure methods make no such claim. */
export interface RandomProvider {
  secure(length: number): Uint8Array;
  insecure(length: number): Uint8Array;
  insecureSeed(): readonly [bigint, bigint];
}

export interface SystemProviders {
  readonly clocks?: ClockProvider | undefined;
  readonly random?: RandomProvider | undefined;
}

const U64 = (1n << 64n) - 1n;
function duration(value: bigint): bigint {
  if (typeof value !== "bigint" || value < 0n || value > U64)
    throw new RangeError("clock provider requires a u64 nanosecond value");
  return value;
}

/** Validate host clocks identically for standard calls and the interpreter's libc imports. */
export function checkedClocks(source: ClockProvider): ClockProvider {
  return {
    systemNow() {
      const value = source.systemNow();
      if (
        typeof value.seconds !== "bigint" ||
        value.seconds < -(1n << 63n) ||
        value.seconds >= 1n << 63n ||
        !Number.isInteger(value.nanoseconds) ||
        value.nanoseconds < 0 ||
        value.nanoseconds >= 1e9
      )
        throw new RangeError(
          "clock provider requires s64 seconds and fractional nanoseconds in [0, 1000000000)",
        );
      return value;
    },
    systemResolution: () => duration(source.systemResolution()),
    monotonicNow: () => duration(source.monotonicNow()),
    monotonicResolution: () => duration(source.monotonicResolution()),
    waitUntil: (when, signal) => source.waitUntil(when, signal),
  };
}

/** Cache seed identity per revision, including when an injected provider generates fresh seeds. */
export function revisionRandom(source: RandomProvider): RandomProvider {
  let seed: readonly [bigint, bigint] | undefined;
  return {
    secure: (length) => source.secure(length),
    insecure: (length) => source.insecure(length),
    insecureSeed() {
      if (seed) return seed;
      const value = source.insecureSeed();
      if (
        value.length !== 2 ||
        value.some((word) => typeof word !== "bigint" || word < 0n || word >= 1n << 64n)
      )
        throw new Error("random provider seed requires two u64 values");
      return (seed = [value[0], value[1]]);
    },
  };
}

/** Largest single timer wait accepted by setTimeout. */
const TIMER_LIMIT = 2 ** 31 - 1;

/** A fresh provider; its clock uses the browser's monotonic performance timeline. */
export function browserClocks(): ClockProvider {
  const now = () => BigInt(Math.floor(performance.now() * 1000)) * 1000n;
  return {
    systemNow() {
      const milliseconds = Date.now();
      const seconds = Math.floor(milliseconds / 1000);
      return { seconds: BigInt(seconds), nanoseconds: (milliseconds - seconds * 1000) * 1_000_000 };
    },
    // Browser privacy settings can coarsen these clocks further; embedders can supply their
    // actual resolution through a provider. No sub-millisecond precision is promised here.
    systemResolution: () => 1_000_000n,
    monotonicNow: now,
    monotonicResolution: () => 1_000_000n,
    async waitUntil(deadline, signal) {
      if (signal?.aborted) throw cancelled();
      for (let left = deadline - now(); left > 0n; left = deadline - now()) {
        const milliseconds = Math.min(TIMER_LIMIT, Math.ceil(Number(left) / 1e6));
        await new Promise<void>((resolve, reject) => {
          if (signal?.aborted) return reject(cancelled());
          const abort = () => {
            clearTimeout(timer);
            reject(cancelled());
          };
          const timer = setTimeout(() => {
            signal?.removeEventListener("abort", abort);
            resolve();
          }, milliseconds);
          signal?.addEventListener("abort", abort, { once: true });
        });
      }
    },
  };
}

/** The default never substitutes Math.random when cryptographic randomness is unavailable. */
export function browserRandom(): RandomProvider {
  const secure = (length: number) => {
    const bytes = new Uint8Array(length);
    for (let offset = 0; offset < length; offset += 65536)
      globalThis.crypto.getRandomValues(bytes.subarray(offset, offset + 65536));
    return bytes;
  };
  const insecureSeed = () => {
    const view = new DataView(secure(16).buffer);
    return [view.getBigUint64(0, true), view.getBigUint64(8, true)] as const;
  };
  return { secure, insecure: secure, insecureSeed };
}

/** Require exact provider output before a value can be encoded or seed the VM. */
function bytes(provider: (length: number) => Uint8Array, length: number): Uint8Array {
  const result = provider(length);
  if (!(result instanceof Uint8Array) || result.length !== length)
    throw new Error("random provider returned the wrong byte length");
  return result;
}

/** One method set per revision: in particular, get-insecure-seed stays stable for that Store. */
export function systemMethods(
  clocks: ClockProvider,
  random: RandomProvider,
): Record<string, Implementation> {
  const randomBytes =
    (secure: boolean): Implementation =>
    (call, [length]: [bigint]) => {
      const bound = BigInt(Math.max(0, call.host.packetBytes - 4));
      return bytes(
        (size) => (secure ? random.secure(size) : random.insecure(size)),
        Number(length > bound ? bound : length),
      );
    };
  const randomU64 = (secure: boolean) => () => {
    const result = bytes((size) => (secure ? random.secure(size) : random.insecure(size)), 8);
    return new DataView(result.buffer, result.byteOffset, result.byteLength).getBigUint64(0, true);
  };
  return {
    "wasi:random/random@0.3.0#getRandomBytes": randomBytes(true),
    "wasi:random/random@0.3.0#getRandomU64": randomU64(true),
    "wasi:random/insecure@0.3.0#getInsecureRandomBytes": randomBytes(false),
    "wasi:random/insecure@0.3.0#getInsecureRandomU64": randomU64(false),
    "wasi:random/insecure-seed@0.3.0#getInsecureSeed": () => random.insecureSeed(),
    "wasi:clocks/system-clock@0.3.0#now": () => clocks.systemNow(),
    "wasi:clocks/system-clock@0.3.0#getResolution": () => clocks.systemResolution(),
    "wasi:clocks/monotonic-clock@0.3.0#now": () => clocks.monotonicNow(),
    "wasi:clocks/monotonic-clock@0.3.0#getResolution": () => clocks.monotonicResolution(),
    "wasi:clocks/monotonic-clock@0.3.0#waitUntil": ({ signal, host }, [when]: [bigint]) =>
      abortable(
        host.physical(signal, () => clocks.waitUntil(when, signal)),
        signal,
      ),
    "wasi:clocks/monotonic-clock@0.3.0#waitFor": ({ signal, host }, [duration]: [bigint]) =>
      abortable(
        host.physical(signal, () => clocks.waitUntil(clocks.monotonicNow() + duration, signal)),
        signal,
      ),
  };
}
