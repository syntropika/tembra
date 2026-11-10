// Apply host-issued authority before author code runs; the closures remain in captured heaps.
(() => {
  const { clock, seed } = globalThis.__tembraBuiltins!;
  delete globalThis.__tembraBuiltins;
  const NativeDate = Date;
  const requireClock = () => {
    if (!clock) throw new Error("clock capability is not granted");
  };

  // Replacing only Date.now leaves Date() and instance.constructor as ambient clock paths.
  function ScopedDate(...values: unknown[]): Date | string {
    if (!new.target) {
      requireClock();
      return NativeDate();
    }
    if (!values.length) requireClock();
    return Reflect.construct(NativeDate, values, new.target);
  }
  ScopedDate.prototype = NativeDate.prototype;
  Object.defineProperty(NativeDate.prototype, "constructor", {
    value: ScopedDate,
    writable: true,
    configurable: true,
  });
  Object.defineProperties(ScopedDate, {
    name: { value: "Date", configurable: true },
    parse: { value: NativeDate.parse, writable: true, configurable: true },
    UTC: { value: NativeDate.UTC, writable: true, configurable: true },
    now: {
      value: () => {
        requireClock();
        return NativeDate.now();
      },
      writable: true,
      configurable: true,
    },
  });
  Object.defineProperty(globalThis, "Date", {
    value: ScopedDate,
    writable: true,
    configurable: true,
  });
  // Even if a native method is reachable through a prototype, its libc clock import is
  // independently mediated by the host. Pure date construction and arithmetic remain usable.
  if (typeof performance !== "undefined") {
    const nativeNow = performance.now.bind(performance);
    Object.defineProperty(performance, "now", {
      value: () => {
        requireClock();
        return nativeNow();
      },
      writable: true,
      configurable: true,
    });
  }

  if (seed === null) {
    Math.random = () => {
      throw new Error("random capability is not granted");
    };
  } else {
    // xorshift128+ is deliberately insecure. Secure entropy uses wasi:random/random.
    // Its entire state is JavaScript heap state, so snapshots continue rather than reseed it.
    let left = BigInt(seed[0]);
    let right = BigInt(seed[1]);
    if (left === 0n && right === 0n) right = 1n;
    const mask = (1n << 64n) - 1n;
    Math.random = () => {
      let next = left;
      const previous = right;
      left = previous;
      next ^= (next << 23n) & mask;
      right = (next ^ previous ^ (next >> 17n) ^ (previous >> 26n)) & mask;
      return Number(((right + previous) & mask) >> 11n) / 9007199254740992;
    };
  }
})();
