import { expect, test } from "bun:test";
import { BrowserPort } from "../src/interpreter/browser-port.ts";
import type { Guest } from "../src/interpreter/core.ts";
import { memoryFilesystem } from "../src/interpreter/filesystem.ts";
import { StandardHost } from "../src/interpreter/standard-host.ts";
import { METHODS } from "../src/interpreter/standard.ts";
import {
  CALL,
  STREAM_READ,
  Writer,
  encode,
  decode,
  Reader,
  type HandleCodec,
} from "../src/interpreter/standard-codec.ts";
import type { ExecutionScope } from "../src/interpreter/types.ts";
import { TembraError } from "../src/error.ts";
import { Code } from "../src/bridge/identifiers.ts";

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
function guest(exports: Partial<Guest>): Guest {
  return {
    load: () => ({ ok: true, value: "{}", interrupted: false }),
    declare: () => ({ ok: true, value: "null", interrupted: false }),
    start: () => ({
      outcome: { ok: true, value: "42", interrupted: false },
      effects: [],
    }),
    resume: () => {
      throw new Error("Unexpected continuation");
    },
    cancel: () => ({ ok: false, value: "cancelled", interrupted: true }),
    ...exports,
  };
}
/** Issue a scope without ambient host authority. */
function scope(exports: Partial<ExecutionScope> = {}): ExecutionScope {
  return {
    check() {},
    cancel() {},
    deliver(_request, value, publish) {
      publish(value);
    },
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
        { id: 1, packet: JSON.stringify({ operation: "wait", milliseconds: 1 }) },
        { id: 2, packet: JSON.stringify({ operation: "wait", milliseconds: 2 }) },
      ],
    }),
    resume: (completed) => {
      resumes++;
      expect(completed[0]?.id).toBe(1);
      return { outcome: { ok: true, value: "42", interrupted: false }, effects: [] };
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
  expect(await operation).toBe("42");
  expect(resumes).toBe(1);
  await port.close();
  expect(disposed).toBe(1);
});

test("a hard deadline waits for admitted host I/O before disposal", async () => {
  const physical = deferred<void>(),
    entered = deferred<void>(),
    ended = deferred<void>();
  let cancelled = 0,
    observed = false;
  const port = new BrowserPort(
    10,
    async () => {},
    () => {},
  );
  port.guest = guest({
    start: () => ({
      effects: [
        {
          id: 1,
          packet: JSON.stringify({ operation: "wait", milliseconds: 1 }),
        },
      ],
    }),
    cancel: () => {
      cancelled++;
      return { ok: false, value: "cancelled", interrupted: true };
    },
  });
  const operation = port
    .execute(
      "{}",
      scope({
        cancel: () => ended.resolve(),
        async: async () => {
          entered.resolve();
          await physical.promise;
          return null;
        },
      }),
    )
    .then(
      () => {
        observed = true;
        return undefined;
      },
      (error) => {
        observed = true;
        return error;
      },
    );
  await entered.promise;
  await ended.promise;
  await expect(port.close()).rejects.toThrow("physical work");
  expect(observed).toBe(false);
  physical.resolve();
  expect(String(await operation)).toContain("wall time limit");
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
        ? { effects: [{ id: 1, packet: JSON.stringify({ operation: "wait", milliseconds: 1 }) }] }
        : { outcome: { ok: true, value: "42", interrupted: false }, effects: [] },
    resume: (completed) => {
      expect(completed[0]?.ok).toBe(false);
      return {
        outcome: { ok: false, value: "operation cancelled", interrupted: false },
        effects: [],
      };
    },
    cancel: () => {
      cancelled++;
      return { ok: false, value: "hard cancellation", interrupted: true };
    },
  });
  await expect(
    port.execute("{}", scope({ async: () => Promise.reject(new Error("operation cancelled")) })),
  ).rejects.toThrow("operation cancelled");
  expect(await port.execute("{}", scope())).toBe("42");
  expect(cancelled).toBe(0);
  await port.close();
});

test("host sync normalizes malformed and oversized packets to string errors", () => {
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
      effects: [1, 2].map((id) => ({
        id,
        packet: JSON.stringify({ operation: "wait", milliseconds: 1 }),
      })),
    }),
    resume: (completed) => {
      resumes++;
      expect(completed.map((value) => value.id)).toEqual([1, 2]);
      return { outcome: { ok: true, value: "42", interrupted: false }, effects: [] };
    },
  });
  expect(await port.execute("{}", scope({ async: async () => null }))).toBe("42");
  expect(resumes).toBe(1);
  await port.close();
});

test("an unpublished real standard read is poisoned through the browser port's callback cleanup", async () => {
  const FS = "wasi:filesystem/types@0.3.0",
    PREOPENS = "wasi:filesystem/preopens@0.3.0";
  const filesystem = memoryFilesystem({ "data.bin": new Uint8Array(150_000) }).open("/data", false);
  const standard = new StandardHost(
    new Set([FS, PREOPENS]),
    [{ path: "/data", writable: false, filesystem }],
    8 << 20,
  );
  const raw: HandleCodec = {
    handle: (_kind, _detail, value) => value as number,
    value: (_kind, _detail, value) => value,
  };
  const method = (name: string) => METHODS.findIndex((item) => item.name === name);
  const call = (name: string, ...args: unknown[]) => {
    const index = method(name),
      writer = new Writer();
    writer.u8(0);
    writer.u8(CALL);
    writer.u32(index);
    METHODS[index]!.params.forEach((type, index) => encode(writer, type, args[index], raw));
    return writer.finish();
  };
  const result = (name: string, bytes: Uint8Array): any => {
    const reader = new Reader(bytes);
    const value = decode(reader, METHODS[method(name)]!.result!, raw);
    reader.end();
    return value;
  };
  const [[root]] = result("getDirectories", standard.sync(call("getDirectories")));
  const file = result(
    "openAt",
    (await standard.async(call("openAt", root, {}, "data.bin", {}, { read: true }))).bytes,
  ).val;
  const [stream] = result("readViaStream", standard.sync(call("readViaStream", file, 0n)));
  const writer = new Writer();
  writer.u8(0);
  writer.u8(STREAM_READ);
  writer.handle(stream);
  writer.u32(65536);
  const packet = writer.finish();
  const produced = deferred<void>(),
    ended = deferred<void>(),
    publish = deferred<void>();
  const original = standard.async.bind(standard);
  // Delay only the receipt, after the real host has already advanced the actual file stream.
  standard.async = async (...args) => {
    const delivery = await original(...args);
    produced.resolve();
    await publish.promise;
    return delivery;
  };
  let calls = 0;
  const port = new BrowserPort(
    10000,
    () => filesystem.dispose(),
    () => {},
  );
  port.standard = standard;
  port.guest = guest({
    start: () => ({
      effects:
        ++calls === 1
          ? [
              { id: 1, packet: JSON.stringify({ operation: "wait", milliseconds: 1 }) },
              { id: 2, packet },
            ]
          : [{ id: 3, packet }],
    }),
    resume(completed) {
      if (calls === 1) {
        expect(completed.map((item) => item.id)).toEqual([1]);
        return { outcome: { ok: true, value: "42", interrupted: false }, effects: [] };
      }
      expect(completed[0]?.ok).toBe(false);
      return {
        outcome: { ok: true, value: String(completed[0]?.value), interrupted: false },
        effects: [],
      };
    },
  });
  const operation = port.execute(
    "{}",
    scope({
      async: async () => {
        await produced.promise;
        return 42;
      },
      cancel: () => ended.resolve(),
    }),
  );
  await ended.promise;
  publish.resolve();
  expect(await operation).toBe("42");
  standard.async = original;
  expect(await port.execute("{}", scope())).toBe("stream read was abandoned");
  await port.close();
  expect(standard.size).toBe(0);
});

/** Hold this thread, as a busy guest step does: nothing else runs, the deadline's timer included. */
function hold(milliseconds: number) {
  const until = performance.now() + milliseconds;
  while (performance.now() < until) {
    // Spin.
  }
}

test("a result after the callback's deadline is a timeout, even before its late timer runs", async () => {
  const port = new BrowserPort(
    20,
    async () => {},
    () => {},
  );
  port.guest = guest({
    start: () => {
      hold(50);
      return { outcome: { ok: true, value: "42", interrupted: false }, effects: [] };
    },
  });
  await expect(port.execute("{}", scope())).rejects.toThrow("wall time limit");
  await expect(port.execute("{}", scope())).rejects.toThrow("closed or busy");
  await port.close();
});

test("a guest failure after the callback's deadline is a timeout too", async () => {
  const port = new BrowserPort(
    20,
    async () => {},
    () => {},
  );
  port.guest = guest({
    start: () => ({
      effects: [{ id: 1, packet: JSON.stringify({ operation: "wait", milliseconds: 1 }) }],
    }),
    resume: () => {
      hold(50);
      return {
        outcome: { ok: false, value: "the guest's own failure", interrupted: false },
        effects: [],
      };
    },
  });
  await expect(port.execute("{}", scope({ async: async () => null }))).rejects.toThrow(
    "wall time limit",
  );
  await expect(port.execute("{}", scope())).rejects.toThrow("closed or busy");
  await port.close();
});

test("a guest failure before the callback's deadline keeps its own error", async () => {
  let calls = 0;
  const port = new BrowserPort(
    10000,
    async () => {},
    () => {},
  );
  port.guest = guest({
    start: () => ({
      outcome:
        ++calls === 1
          ? { ok: false, value: "the guest's own failure", interrupted: false }
          : { ok: true, value: "42", interrupted: false },
      effects: [],
    }),
  });
  await expect(port.execute("{}", scope())).rejects.toThrow("the guest's own failure");
  expect(await port.execute("{}", scope())).toBe("42");
  await port.close();
});

/** The refusal a dependency answers, with its code. */
function bridgeClosed(): TembraError {
  return new TembraError({
    code: Code.Forbidden,
    reason: "bridge_closed",
    message: "the bridge refuses",
  });
}
const failed = (value: string) => ({
  outcome: { ok: false, value, interrupted: false },
  effects: [],
});
const WAIT = JSON.stringify({ operation: "wait", milliseconds: 1 });
const ADDRESS = '{"operation":"address"}';

test("a callback that fails with an asynchronous refusal unchanged answers that refusal", async () => {
  const refusal = bridgeClosed();
  let named = "";
  const port = new BrowserPort(
    10000,
    async () => {},
    () => {},
  );
  port.guest = guest({
    start: () => ({ effects: [{ id: 1, packet: WAIT }] }),
    resume: (completed) => {
      expect(completed[0]?.ok).toBe(false);
      // The guest runtime names the refusal it rethrows by its effect's id.
      if (named) port.sync(named);
      return failed(String(completed[0]?.value));
    },
  });
  const refused = scope({ async: () => Promise.reject(refusal) });
  named = '{"operation":"refused","effect":1}';
  await expect(port.execute("{}", refused)).rejects.toBe(refusal);
  // Its own error with the same text answers that text only.
  named = "";
  const forged = await port.execute("{}", refused).catch((error: unknown) => error);
  expect(forged).not.toBe(refusal);
  expect(String(forged)).toContain("the bridge refuses");
  await port.close();
});

test("a callback that fails with a synchronous refusal unchanged answers that refusal", async () => {
  const refusal = bridgeClosed();
  const port = new BrowserPort(
    10000,
    async () => {},
    () => {},
  );
  port.guest = guest({
    start: () => {
      expect(() => port.sync(ADDRESS)).toThrow("the bridge refuses");
      // Asked at once, the port names the request it refused.
      const request = port.sync('{"operation":"refusal"}');
      expect(port.sync('{"operation":"refusal"}')).toBe("null");
      port.sync(`{"operation":"refused","request":${request}}`);
      return failed("forbidden/bridge_closed: the bridge refuses");
    },
  });
  const refused = scope({
    sync: () => {
      throw refusal;
    },
  });
  await expect(port.execute("{}", refused)).rejects.toBe(refusal);
  await port.close();
});

test("a callback names only a refusal it received itself", async () => {
  const refusal = bridgeClosed();
  let calls = 0;
  const port = new BrowserPort(
    10000,
    async () => {},
    () => {},
  );
  port.guest = guest({
    start: () => {
      if (++calls === 1) {
        expect(() => port.sync(ADDRESS)).toThrow("the bridge refuses");
        port.sync('{"operation":"refusal"}');
        return { outcome: { ok: true, value: "0", interrupted: false }, effects: [] };
      }
      // The refusal of the first callback, and a request never refused.
      port.sync('{"operation":"refused","request":1}');
      return failed("forbidden/bridge_closed: the bridge refuses");
    },
  });
  const refused = scope({
    sync: () => {
      throw refusal;
    },
  });
  expect(await port.execute("{}", refused)).toBe("0");
  const stale = await port.execute("{}", refused).catch((error: unknown) => error);
  expect(stale).not.toBe(refusal);
  await port.close();
});
