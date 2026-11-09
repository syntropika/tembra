import { expect, test } from "bun:test";
import { createHost, orb } from "../src/node.ts";
import { KernelOp } from "../src/protocol.ts";

test("fused call completion uses the configured host clock", async () => {
  let now = 100;
  const seen: (number | undefined)[] = [];
  const host = createHost({
    now: () => now,
    clockOrigin: 0,
    native: {
      hasOrb: () => false,
      activate: async () => {},
      invoke: async () => {},
      dispose: async () => {},
      effect: async () => {},
      value: () => undefined,
      finish(call: number, clock?: number) {
        seen.push(clock);
        host.session.kernel.command(KernelOp.Tick, { now: clock ?? 0 });
        host.session.kernel.command(KernelOp.FinishCall, { call });
        return {
          actions: host.session.kernel.command(KernelOp.Drain),
          deadline: host.session.kernel.command(KernelOp.NextDeadline),
        };
      },
    },
  });
  await host.install(
    orb({
      name: "clock",
      apiVersion: 1,
      services: {
        clock: {
          methods: {
            read() {
              now = 123;
              return 42;
            },
          },
        },
      },
    }),
  );
  const reference = await host.get("clock", "main");
  try {
    expect(await reference.read()).toBe(42);
    expect(seen).toEqual([123]);
  } finally {
    reference.release();
    await host.close();
  }
});

test("late native activation rejects after the reserved object has retired", async () => {
  const host = createHost();
  await host.install(orb({ name: "late", apiVersion: 1, services: { late: { methods: {} } } }));
  const address = { service: "late", key: ["main"], principal: "reader" };
  const acquired = host.session.kernel.command(KernelOp.Acquire, {
    ...address,
    apiVersion: 1,
    methods: [],
  });
  host.session.kernel.command(KernelOp.Release, { reference: acquired.reference });
  host.session.kernel.command(KernelOp.Forget, { reference: acquired.reference });
  host.session.lifecycle.flush();
  await Promise.allSettled([...host.session.jobs]);
  try {
    await expect(host.session.handles.activateAcquired(acquired, address)).rejects.toThrow();
  } finally {
    await host.close();
  }
});

test("warm reads settle their root without separate drain/deadline or generic check exchanges", async () => {
  const host = createHost();
  await host.install(
    orb({
      name: "settlement",
      apiVersion: 1,
      services: { settlement: { methods: { read: () => 42 } } },
    }),
  );
  const reference = await host.get("settlement", "main");
  await reference.read();
  const port = host.session.kernel.port;
  const original = port.command.bind(port);
  const check = port.check.bind(port);
  const commands: string[] = [];
  let checks = 0;
  port.command = (op, input) => {
    commands.push(op);
    return original(op, input);
  };
  port.check = (root, call) => {
    checks++;
    return check(root, call);
  };
  try {
    expect(await reference.read()).toBe(42);
    expect(commands.filter((op) => op === KernelOp.EndRootBatch)).toHaveLength(1);
    expect(commands.filter((op) => op === KernelOp.FinishCallBatch)).toHaveLength(1);
    for (const op of [
      KernelOp.EndRoot,
      KernelOp.Drain,
      KernelOp.NextDeadline,
      KernelOp.CheckContext,
    ])
      expect(commands).not.toContain(op);
    expect(checks).toBeGreaterThan(0);
  } finally {
    port.command = original;
    port.check = check;
    reference.release();
    await host.close();
  }
});
