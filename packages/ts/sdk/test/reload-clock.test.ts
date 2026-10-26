import { expect, test } from "bun:test";
import { createHost } from "../src/node.ts";
import { Code, Reason } from "../src/identifiers.ts";
import { KernelOp } from "../src/protocol.ts";
import { deferred } from "../src/runtime/records.ts";
import type { OrbDefinition } from "../src/types.ts";

for (const origin of [0, 1_700_000_000_000]) {
  test(`an idle host gives an asynchronous reload its full deadline from clock origin ${origin}`, async () => {
    let now = origin;
    const host = createHost({ now: () => now });
    const opened: number[] = [];
    const closed: number[] = [];
    const retired = deferred<void>();
    let deadline: number | undefined;
    const definition = (version: number): OrbDefinition => ({
      name: "idle-reload",
      apiVersion: 1,
      create(self) {
        self.resource("connection", {
          version,
          async open() {
            if (version === 2) {
              deadline = host.session.kernel.command<number>(KernelOp.NextDeadline);
              await new Promise((resolve) => setTimeout(resolve, 0));
              now += 10;
              host.session.kernel.command(KernelOp.Tick, { now: host.session.now() });
              host.session.lifecycle.flush();
            }
            opened.push(version);
            return version;
          },
          close(value: number) {
            closed.push(value);
            if (value === 1) retired.resolve();
          },
        });
      },
    });
    try {
      await host.install(definition(1));
      now = origin + 10_000;
      await host.reload([definition(2)], { deadline: 50 });
      await retired.promise;
      expect(deadline).toBe(10_050);
      expect(opened).toEqual([1, 2]);
      expect(closed).toEqual([1]);
      expect(host.session.kernel.command(KernelOp.NextDeadline)).toBe(null);
    } finally {
      await host.close();
    }
    expect(closed).toEqual([1, 2]);
  });
}

test("an idle host still expires a slow reload and waits for physical settlement", async () => {
  let now = 0;
  const host = createHost({ now: () => now });
  const entered = deferred<void>();
  const finish = deferred<void>();
  const closed: number[] = [];
  let cancelled = false;
  let settled = false;
  const definition = (version: number): OrbDefinition => ({
    name: "idle-deadline",
    apiVersion: 1,
    create(self) {
      self.resource("connection", {
        version,
        async open(signal: AbortSignal) {
          if (version === 2) {
            signal.addEventListener("abort", () => (cancelled = true), { once: true });
            entered.resolve();
            await finish.promise;
          }
          return version;
        },
        close(value: number) {
          closed.push(value);
        },
      });
    },
  });
  let replacing: Promise<unknown> | undefined;
  try {
    await host.install(definition(1));
    now = 10_000;
    replacing = host.reload([definition(2)], { deadline: 50 }).then(
      () => {
        settled = true;
      },
      (error: unknown) => {
        settled = true;
        return error;
      },
    );
    await entered.promise;
    now = 10_049;
    host.session.kernel.command(KernelOp.Tick, { now });
    host.session.lifecycle.flush();
    expect(cancelled).toBe(false);
    now = 10_050;
    host.session.kernel.command(KernelOp.Tick, { now });
    host.session.lifecycle.flush();
    expect(cancelled).toBe(true);
    expect(settled).toBe(false);
    expect(closed).toEqual([]);
    finish.resolve();
    expect(await replacing).toMatchObject({ code: Code.Conflict, reason: Reason.ReloadDeadline });
    expect(closed).toEqual([2]);
    expect(host.session.lifecycle.diagnostics().resources).toBe(1);
  } finally {
    finish.resolve();
    await replacing;
    await host.close();
  }
  expect(closed).toEqual([2, 1]);
});
