import { expect, test } from "bun:test";
import type { Context } from "../src/runtime/context.ts";
import {
  ByteStreamOwner,
  issueByteStream,
  receiveByteStream,
} from "../src/interpreter/component/streams.ts";
import { STREAM_CHUNK } from "../src/interpreter/component/wasi-host.ts";

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<T>((yes, no) => {
    resolve = yes;
    reject = no;
  });
  return { promise, resolve, reject };
}

function receiver(owner: ByteStreamOwner, ended = new AbortController()) {
  const work: Promise<unknown>[] = [];
  const failures: unknown[] = [];
  const iterator = receiveByteStream(
    owner,
    ended.signal,
    (operation) => work.push(Promise.resolve(operation())),
    (error) => failures.push(error),
  );
  return { iterator, work, failures, ended };
}

test("a stream head is demand driven and clean EOF waits for physical completion", async () => {
  const physical = deferred<void>();
  const reachedEof = deferred<void>();
  const chunk = new Uint8Array([1, 2, 3]);
  const requests: number[] = [];
  let disposed = 0;
  const owner = issueByteStream(
    {
      async read({ count }: { count: number }) {
        requests.push(count);
        if (requests.length === 1) return { value: chunk, done: false };
        reachedEof.resolve();
        return { done: true };
      },
      [Symbol.dispose]() {
        disposed++;
      },
    },
    physical.promise,
    new AbortController().signal,
  );
  const { iterator, work, failures } = receiver(owner);
  expect(requests).toEqual([]);
  expect((await iterator.next()).value).toEqual(chunk);
  chunk[0] = 9;
  let eof = false;
  const read = iterator.next().then((result) => {
    eof = true;
    return result;
  });
  await reachedEof.promise;
  expect(eof).toBe(false);
  expect(requests).toEqual([STREAM_CHUNK, STREAM_CHUNK]);
  physical.resolve();
  expect((await read).done).toBe(true);
  await Promise.all(work);
  expect(disposed).toBe(1);
  expect(failures).toEqual([]);
});

test("writer EOF followed by a physical trap is reported as failed delivery", async () => {
  const physical = deferred<void>();
  const eof = deferred<void>();
  const owner = issueByteStream(
    {
      async read() {
        eof.resolve();
        return { done: true };
      },
    },
    physical.promise,
    new AbortController().signal,
  );
  const { iterator, failures, work } = receiver(owner);
  const read = iterator.next();
  await eof.promise;
  const trap = new Error("producer trapped after writer EOF");
  physical.reject(trap);
  expect((await read).done).toBe(true);
  await Promise.all(work);
  expect(failures).toEqual([trap]);
});

test("early drop interrupts a quiet read and retains physical cleanup independently", async () => {
  const physical = deferred<void>();
  const reading = deferred<void>();
  let disposed = 0;
  const owner = issueByteStream(
    {
      read() {
        reading.resolve();
        return new Promise(() => {});
      },
      [Symbol.dispose]() {
        disposed++;
      },
    },
    physical.promise,
    new AbortController().signal,
  );
  const { iterator, work, failures } = receiver(owner);
  const read = iterator.next();
  await reading.promise;
  expect((await iterator.return!()).done).toBe(true);
  expect((await read).done).toBe(true);
  expect(disposed).toBe(1);
  expect(work).toHaveLength(1);
  let cleaned = false;
  void work[0]!.then(() => {
    cleaned = true;
  });
  expect(cleaned).toBe(false);
  const trap = new Error("late physical failure after drop");
  physical.reject(trap);
  await Promise.all(work);
  expect(failures).toEqual([trap]);
  owner.close();
  expect(disposed).toBe(1);
});

test("receiver cancellation drops an unread source and joins its producer", async () => {
  const physical = deferred<void>();
  let reads = 0;
  let disposed = 0;
  const owner = issueByteStream(
    {
      async read() {
        reads++;
        return { done: true };
      },
      [Symbol.dispose]() {
        disposed++;
      },
    },
    physical.promise,
    new AbortController().signal,
  );
  const { iterator, ended, work } = receiver(owner);
  ended.abort();
  expect(disposed).toBe(1);
  expect(reads).toBe(0);
  expect((await iterator.next()).done).toBe(true);
  expect(work).toHaveLength(1);
  physical.resolve();
  await Promise.all(work);
});

test("stream capabilities are branded, consumed once, and bound to one root", async () => {
  const ended = new AbortController();
  let disposed = 0;
  const owner = issueByteStream(
    {
      async read() {
        return { done: true };
      },
      [Symbol.dispose]() {
        disposed++;
      },
    },
    Promise.resolve(),
    new AbortController().signal,
  );
  const context = {
    session: {},
    root: 1,
    signal: ended.signal,
    check() {},
  } as unknown as Context;
  owner.bind(context);
  expect(() => owner.bind({ ...context, root: 2 } as Context)).toThrow("another root");
  const { iterator } = receiver(owner);
  expect(() => receiver(owner)).toThrow("already consumed");
  expect(() => receiver({ kind: "bytes" } as ByteStreamOwner)).toThrow("Foreign");
  ended.abort();
  expect(disposed).toBe(1);
  expect((await iterator.next()).done).toBe(true);
});

test("concurrent demand is serialized and a produced chunk is copied before transfer", async () => {
  const chunk = deferred<{ value: Uint8Array }>();
  let reads = 0;
  const bytes = new Uint8Array([4, 5]);
  const owner = issueByteStream(
    {
      read() {
        reads++;
        return reads === 1 ? chunk.promise : Promise.resolve({ done: true });
      },
    },
    Promise.resolve(),
    new AbortController().signal,
  );
  const { iterator } = receiver(owner);
  const first = iterator.next();
  const second = iterator.next();
  await Promise.resolve();
  expect(reads).toBe(1);
  chunk.resolve({ value: bytes });
  const delivered = (await first).value;
  bytes[0] = 99;
  expect(delivered).toEqual(new Uint8Array([4, 5]));
  expect((await second).done).toBe(true);
  expect(reads).toBe(2);
});

test("producer retirement interrupts the wait after writer EOF without claiming physical exit", async () => {
  const physical = deferred<void>();
  const eof = deferred<void>();
  const retired = new AbortController();
  const owner = issueByteStream(
    {
      async read() {
        eof.resolve();
        return { done: true };
      },
    },
    physical.promise,
    retired.signal,
  );
  const { iterator, work, failures } = receiver(owner);
  const read = iterator.next();
  await eof.promise;
  retired.abort();
  expect((await read).done).toBe(true);
  expect(work).toHaveLength(1);
  let exited = false;
  void owner.finished.then(() => {
    exited = true;
  });
  expect(exited).toBe(false);
  physical.resolve();
  await Promise.all(work);
  expect(failures).toEqual([]);
});

test("an oversized producer chunk fails instead of crossing the transport bound", async () => {
  let disposed = 0;
  const owner = issueByteStream(
    {
      async read() {
        return { value: new Uint8Array(STREAM_CHUNK + 1), done: false };
      },
      [Symbol.dispose]() {
        disposed++;
      },
    },
    Promise.resolve(),
    new AbortController().signal,
  );
  const { iterator, work, failures } = receiver(owner);
  expect((await iterator.next()).done).toBe(true);
  await Promise.all(work);
  expect(failures).toHaveLength(1);
  expect(String(failures[0])).toContain("64 KiB bound");
  expect(disposed).toBe(1);
});
