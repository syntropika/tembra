import { expect, spyOn, test } from "bun:test";
import { createHost } from "../src/node.ts";
import { facet } from "../src/facet.ts";
import { KernelOp } from "../src/protocol.ts";
import { Code, Reason } from "../src/identifiers.ts";
import { deferred } from "../src/runtime/records.ts";

test("linger scheduling preserves unchanged deadlines and replaces earlier or later deadlines", async () => {
  let now = 0;
  const host = createHost({ now: () => now });
  await host.install(
    facet({
      name: "timers",
      apiVersion: 1,
      services: {
        slow: { linger: 100_000, methods: {} },
        fast: { linger: 10_000, methods: {} },
      },
    }),
  );
  let first = await host.get("slow", "first");
  const second = await host.get("slow", "second");
  let fast = await host.get("fast", "main");
  const scheduled = spyOn(globalThis, "setTimeout");
  const cleared = spyOn(globalThis, "clearTimeout");
  try {
    first.release();
    expect(scheduled.mock.calls.map((call) => call[1])).toEqual([100_000]);
    for (now = 1; now <= 10; now++) host.session.lifecycle.flush();
    expect(scheduled).toHaveBeenCalledTimes(1);
    now = 1000;
    second.release();
    expect(scheduled).toHaveBeenCalledTimes(1);
    now = 2000;
    first = await host.get("slow", "first");
    host.session.lifecycle.flush();
    expect(scheduled.mock.calls.at(-1)?.[1]).toBe(99_000);
    expect(scheduled).toHaveBeenCalledTimes(2);
    now = 3000;
    first.release();
    expect(scheduled).toHaveBeenCalledTimes(2);
    now = 4000;
    fast.release();
    expect(scheduled.mock.calls.at(-1)?.[1]).toBe(10_000);
    expect(scheduled).toHaveBeenCalledTimes(3);
    now = 5000;
    fast = await host.get("fast", "main");
    host.session.lifecycle.flush();
    expect(scheduled.mock.calls.at(-1)?.[1]).toBe(96_000);
    expect(scheduled).toHaveBeenCalledTimes(4);
    first = await host.get("slow", "first");
    const retainedSecond = await host.get("slow", "second");
    host.session.lifecycle.flush();
    expect(cleared).toHaveBeenCalledTimes(4);
    expect(scheduled).toHaveBeenCalledTimes(4);
    retainedSecond.release();
  } finally {
    first.release();
    fast.release();
    await host.close();
    scheduled.mockRestore();
    cleared.mockRestore();
  }
});

test("linger retains the same running task and expiry waits for its physical settlement", async () => {
  let now = 0;
  let starts = 0;
  let creates = 0;
  let closes = 0;
  let cancellations = 0;
  const running = deferred<AbortSignal>();
  const finish = deferred<void>();
  const host = createHost({ now: () => now });
  await host.install(
    facet({
      name: "linger-task",
      apiVersion: 1,
      services: {
        worker: {
          linger: 100_000,
          create(self) {
            const instance = ++creates;
            self.resource("connection", {
              open: () => 1,
              close: () => {
                closes++;
              },
            });
            self.task(async ({ signal }) => {
              starts++;
              signal.addEventListener(
                "abort",
                () => {
                  cancellations++;
                },
                { once: true },
              );
              running.resolve(signal);
              await finish.promise;
            });
            return instance;
          },
          methods: { read: (instance) => instance },
        },
      },
    }),
  );
  try {
    let ref = await host.get("worker", "main");
    const signal = await running.promise;
    expect(await ref.read()).toBe(1);
    ref.release();
    expect(signal.aborted).toBe(false);
    now = 50_000;
    ref = await host.get("worker", "main");
    expect(await ref.read()).toBe(1);
    expect(starts).toBe(1);
    now = 150_000;
    host.session.kernel.command(KernelOp.Tick, { now });
    host.session.lifecycle.flush();
    expect(signal.aborted).toBe(false);
    ref.release();
    now = 249_999;
    host.session.kernel.command(KernelOp.Tick, { now });
    host.session.lifecycle.flush();
    expect(signal.aborted).toBe(false);
    now = 250_000;
    host.session.kernel.command(KernelOp.Tick, { now });
    host.session.lifecycle.flush();
    expect(signal.aborted).toBe(true);
    expect(cancellations).toBe(1);
    expect(closes).toBe(0);
    expect(host.diagnostics()).toMatchObject({ objects: 1, tasks: 1 });
    await expect(host.get("worker", "main")).rejects.toMatchObject({
      code: Code.Unavailable,
      reason: Reason.Disposing,
    });
  } finally {
    finish.resolve();
    await host.close();
  }
  expect(creates).toBe(1);
  expect(closes).toBe(1);
  expect(host.diagnostics()).toMatchObject({ objects: 0, tasks: 0, resources: 0 });
  expect(host.errors()).toEqual([]);
});
