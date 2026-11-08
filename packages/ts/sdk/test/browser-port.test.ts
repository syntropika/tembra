import { expect, test } from "bun:test";
import { BrowserPort, type BrowserGuest } from "../src/interpreter/browser-port.ts";
import type { ExecutionScope } from "../src/interpreter/types.ts";

/** Control physical completion independently of the caller's observation. */
function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<T>((accept, refuse) => {
    resolve = accept;
    reject = refuse;
  });
  return { promise, resolve, reject };
}
/** Supply unrelated exports while each test controls the actual dispatch boundary. */
function guest(exports: Partial<BrowserGuest>): BrowserGuest {
  return {
    load: () => ({ value: { tag: "ok", val: "{}" }, interrupted: false }),
    declare: () => ({ value: { tag: "ok", val: "null" }, interrupted: false }),
    start: () => ({
      outcome: { value: { tag: "ok", val: "42" }, interrupted: false },
      effects: [],
    }),
    resume: () => {
      throw new Error("Unexpected continuation");
    },
    cancel: () => ({ value: { tag: "err", val: "cancelled" }, interrupted: true }),
    ...exports,
  };
}
/** Issue a scope without ambient host authority. */
function scope(exports: Partial<ExecutionScope> = {}): ExecutionScope {
  return {
    check() {},
    cancel() {},
    sync() {
      throw new Error("No synchronous operation expected");
    },
    async() {
      return Promise.reject(new Error("No asynchronous operation expected"));
    },
    ...exports,
  };
}

test("a completed Promise.race retains physical ownership of its losing host operation", async () => {
  const first = deferred<unknown>(),
    losing = deferred<unknown>(),
    entered = deferred<void>(),
    ended = deferred<void>();
  let issued = 0,
    resumes = 0,
    disposed = 0,
    returned = false;
  const port = new BrowserPort(
    10000,
    async () => {
      disposed++;
    },
    () => {},
  );
  port.guest = guest({
    start: () => ({
      effects: [
        { id: 1n, packet: JSON.stringify({ operation: "wait", milliseconds: 1 }) },
        { id: 2n, packet: JSON.stringify({ operation: "wait", milliseconds: 2 }) },
      ],
    }),
    resume: (completed) => {
      resumes++;
      expect(completed[0]?.id).toBe(1n);
      return { outcome: { value: { tag: "ok", val: "42" }, interrupted: false }, effects: [] };
    },
  });
  const operation = port
    .execute(
      "{}",
      scope({
        cancel: () => ended.resolve(),
        async: (request) => {
          if (++issued === 2) entered.resolve();
          return request.operation === "wait" && request.milliseconds === 1
            ? first.promise
            : losing.promise;
        },
      }),
    )
    .then((value) => {
      returned = true;
      return value;
    });
  await entered.promise;
  first.resolve(42);
  await ended.promise;
  expect(returned).toBe(false);
  await expect(port.close()).rejects.toThrow("physical work");
  losing.resolve(0);
  expect(await operation).toBe(42);
  expect(resumes).toBe(1);
  await port.close();
  expect(disposed).toBe(1);
});

test("a hard deadline waits for the Wasm frame and I/O before cancellation and disposal", async () => {
  const frame = deferred<Awaited<ReturnType<BrowserGuest["start"]>>>(),
    physical = deferred<void>(),
    ended = deferred<void>();
  let inWasm = true,
    cancelled = 0,
    observed = false;
  const port = new BrowserPort(
    5,
    async () => {},
    () => {},
  );
  port.io.track(physical.promise);
  port.guest = guest({
    start: () =>
      frame.promise.then((value) => {
        inWasm = false;
        return value;
      }),
    cancel: () => {
      expect(inWasm).toBe(false);
      cancelled++;
      return { value: { tag: "err", val: "cancelled" }, interrupted: true };
    },
  });
  const operation = port.execute("{}", scope({ cancel: () => ended.resolve() })).then(
    (value) => {
      observed = true;
      return { value, error: undefined };
    },
    (error) => {
      observed = true;
      return { value: undefined, error };
    },
  );
  await ended.promise;
  expect(cancelled).toBe(0);
  await expect(port.close()).rejects.toThrow("physical work");
  frame.resolve({ effects: [] });
  await new Promise((resolve) => setTimeout(resolve, 0));
  expect(observed).toBe(false);
  physical.resolve();
  expect((await operation).error.message).toContain("wall time limit");
  expect(cancelled).toBe(1);
  await expect(port.execute("{}", scope())).rejects.toThrow("closed or busy");
  await port.close();
});

test("a rejected cooperative effect settles in the same heap and permits a later call", async () => {
  let calls = 0,
    cancelled = 0;
  const port = new BrowserPort(
    10000,
    async () => {},
    () => {},
  );
  port.guest = guest({
    start: () =>
      ++calls === 1
        ? { effects: [{ id: 1n, packet: JSON.stringify({ operation: "wait", milliseconds: 1 }) }] }
        : { outcome: { value: { tag: "ok", val: "42" }, interrupted: false }, effects: [] },
    resume: (completed) => {
      expect(completed[0]?.value.tag).toBe("err");
      return {
        outcome: { value: { tag: "err", val: "operation cancelled" }, interrupted: false },
        effects: [],
      };
    },
    cancel: () => {
      cancelled++;
      return { value: { tag: "err", val: "hard cancellation" }, interrupted: true };
    },
  });
  await expect(
    port.execute("{}", scope({ async: () => Promise.reject(new Error("operation cancelled")) })),
  ).rejects.toThrow("operation cancelled");
  expect(await port.execute("{}", scope())).toBe(42);
  expect(cancelled).toBe(0);
  await port.close();
});

test("host-sync normalizes malformed and oversized packets to WIT string errors", () => {
  const port = new BrowserPort(
    1000,
    async () => {},
    () => {},
    64,
  );
  for (const packet of ['{"operation":"unknown"}', "x".repeat(65)]) {
    let failure: unknown;
    try {
      port.sync(packet);
    } catch (error) {
      failure = error;
    }
    expect(typeof failure).toBe("string");
  }
});

test("already completed effects resume together", async () => {
  const port = new BrowserPort(
    1000,
    async () => {},
    () => {},
  );
  let resumes = 0;
  port.guest = guest({
    start: () => ({
      effects: [1n, 2n].map((id) => ({
        id,
        packet: JSON.stringify({ operation: "wait", milliseconds: 1 }),
      })),
    }),
    resume: (completed) => {
      resumes++;
      expect(completed.map((value) => value.id)).toEqual([1n, 2n]);
      return { outcome: { value: { tag: "ok", val: "42" }, interrupted: false }, effects: [] };
    },
  });
  expect(await port.execute("{}", scope({ async: async () => null }))).toBe(42);
  expect(resumes).toBe(1);
  await port.close();
});
