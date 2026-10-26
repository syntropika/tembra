import { expect, test } from "bun:test";
import { createHost, orb } from "../src/node.ts";
import { KernelOp } from "../src/protocol.ts";
import type { Action } from "../src/kernel.ts";

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
          actions: host.session.kernel.command<Action[]>(KernelOp.Drain),
          deadline: host.session.kernel.command<number | null>(KernelOp.NextDeadline),
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
