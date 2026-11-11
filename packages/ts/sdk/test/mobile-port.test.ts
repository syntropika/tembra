import { expect, test } from "bun:test";
import { MobileDriver, type MobilePort } from "../src/interpreter/mobile.ts";
import type { ExecutionScope } from "../src/interpreter/types.ts";

/** Supply only the event seam; native execution has separate real-engine qualification. */
function fixture(events: unknown[]) {
  const replies: unknown[] = [];
  let finished = false;
  let cancelled = false;
  const port: MobilePort = {
    description: () => "{}",
    begin() {},
    receive: async () => JSON.stringify(events.shift()),
    reply: (id, packet) => {
      replies.push([id, JSON.parse(packet)]);
      return true;
    },
    finish: () => {
      finished = true;
    },
    abort: async () => {},
    release() {},
    cancel() {},
    close() {},
  };
  const scope: ExecutionScope = {
    check() {},
    cancel: () => {
      cancelled = true;
    },
    deliver(_request, value, publish) {
      publish(value);
    },
    sync: () => 42,
    async: async () => null,
  };
  return {
    port,
    driver: new MobileDriver(port, () => {}),
    scope,
    replies,
    finished: () => finished,
    cancelled: () => cancelled,
  };
}

test("mobile declarations answer synchronous host requests through their issued scope", async () => {
  const f = fixture([
    { event: "host", id: 1, asynchronous: false, packet: '{"operation":"orb"}' },
    { event: "complete", result: { ok: "null" } },
  ]);
  expect(await f.driver.declare("{}", f.scope)).toBeNull();
  expect(f.replies).toEqual([[1, { ok: 42 }]]);
  expect(f.finished()).toBe(true);
  expect(f.cancelled()).toBe(true);
});

test("mobile interruption retains physical ownership until admitted host work settles", async () => {
  let complete!: () => void;
  const operation = new Promise<void>((resolve) => {
    complete = resolve;
  });
  const f = fixture([
    {
      event: "host",
      id: 1,
      asynchronous: true,
      packet: '{"operation":"reference.ready","id":1}',
    },
    { event: "complete", result: { error: "interrupted" } },
  ]);
  f.scope.async = () => operation;
  const result = f.driver.execute("{}", f.scope);
  const outcome = result.then(
    () => undefined,
    (error: unknown) => error,
  );
  await Promise.resolve();
  await Promise.resolve();
  await Promise.resolve();
  expect(f.finished()).toBe(false);
  complete();
  expect(await outcome).toBeInstanceOf(Error);
  expect(String(await outcome)).toContain("interrupted");
  expect(f.finished()).toBe(true);
});

test("mobile factories refuse asynchronous I/O without admitting it", async () => {
  let admitted = false;
  const f = fixture([
    { event: "host", id: 2, asynchronous: true, packet: '{"operation":"wait","milliseconds":1}' },
    { event: "complete", result: { ok: "null" } },
  ]);
  f.scope.async = async () => {
    admitted = true;
  };
  await f.driver.declare("{}", f.scope);
  expect(admitted).toBe(false);
  expect(f.replies).toEqual([
    [2, { error: "Factories cannot perform asynchronous host operations" }],
  ]);
});

test("mobile malformed async requests receive a refusal instead of stranding the guest", async () => {
  const f = fixture([
    {
      event: "host",
      id: 3,
      asynchronous: true,
      packet: '{"operation":"reference.acquire","service":"counter","key":[1]}',
    },
    { event: "complete", result: { ok: "null" } },
  ]);
  let admitted = false;
  f.scope.async = async () => {
    admitted = true;
  };
  expect(await f.driver.execute("{}", f.scope)).toBe("null");
  expect(admitted).toBe(false);
  expect(f.replies[0]).toEqual([
    3,
    { error: "Malformed asynchronous host request: invalid field key" },
  ]);
  expect(f.finished()).toBe(true);
});

test("mobile call results retain original numeric bytes for prepared validation", async () => {
  const f = fixture([{ event: "complete", result: { ok: '{"zero":-0,"n":1.0000000000000002}' } }]);
  expect(await f.driver.execute("{}", f.scope)).toBe('{"zero":-0,"n":1.0000000000000002}');
});

test("mobile receive failures quarantine the native exchange before rejecting", async () => {
  const f = fixture([]);
  let aborted = false;
  f.port.receive = async () => {
    throw new Error("Transport failed");
  };
  f.port.abort = async () => {
    aborted = true;
  };
  await expect(f.driver.execute("{}", f.scope)).rejects.toThrow("Transport failed");
  expect(aborted).toBe(true);
  expect(f.cancelled()).toBe(true);
});

test("mobile cancellation reaches native standard calls during their active exchange", async () => {
  for (const alreadyCancelled of [false, true]) {
    const controller = new AbortController();
    if (alreadyCancelled) controller.abort();
    const f = fixture([{ event: "complete", result: { ok: "null" } }]);
    let cancellations = 0;
    f.port.cancel = () => cancellations++;
    const result = f.driver.execute("{}", { ...f.scope, signal: controller.signal });
    if (!alreadyCancelled) controller.abort();
    await result;
    expect(cancellations).toBe(1);
  }
});

test("mobile completion detaches native cancellation before ending its scope", async () => {
  const controller = new AbortController();
  const f = fixture([{ event: "complete", result: { ok: "null" } }]);
  let cancellations = 0;
  f.port.cancel = () => cancellations++;
  f.scope.cancel = () => controller.abort();
  await f.driver.execute("{}", { ...f.scope, signal: controller.signal });
  expect(cancellations).toBe(0);
});
