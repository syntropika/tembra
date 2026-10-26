import { expect, test } from "bun:test";
import { createHost, ContextKey } from "../src/node.ts";
import { series } from "../src/hooks.ts";
import type { Context } from "../src/runtime/context.ts";
import type { Declaration } from "../src/authoring/declaration.ts";
import { deferred } from "../src/runtime/records.ts";

const Trace = ContextKey.text("test/terminal-trace");
type Hook = ReturnType<Declaration["hook"]>;

for (const outcome of ["Completed", "Failed", "Cancelled"])
  test(`terminal observation awaits permitted work after ${outcome} without reviving its source`, async () => {
    const host = createHost();
    const finished = deferred<void>();
    const seen: unknown[] = [];
    let source!: Context;
    let first!: Hook;
    let second!: Hook;
    let captured!: Context;
    let failure: unknown;
    await host.install([
      {
        name: "execution",
        apiVersion: 1,
        hooks: { terminal: series(), another: series() },
        services: {
          execution: {
            create(self) {
              first = self.hook("terminal");
              second = self.hook("another");
              return {};
            },
            methods: {
              async run(_value: unknown, ctx: Context) {
                source = ctx.with(Trace, "source-only");
                if (outcome === "Cancelled") ctx.cancel();
                try {
                  await first.observe(source, outcome);
                  await expect(second.observe(source, outcome)).rejects.toMatchObject({
                    reason: "observation_admitted",
                  });
                } catch (error) {
                  failure = error;
                } finally {
                  finished.resolve();
                }
                return outcome;
              },
            },
          },
        },
      },
      {
        name: "temporary",
        apiVersion: 1,
        services: {
          temporary: { methods: { read: (_value: unknown, ctx: Context) => ctx.principal } },
        },
      },
      {
        name: "observer",
        apiVersion: 1,
        requires: { execution: { apiVersion: 1, hooks: ["terminal"] } },
        contributes: {
          execution: {
            terminal: async (ctx, value) => {
              captured = ctx;
              expect(ctx.principal).toBe("alice");
              expect(ctx.value(Trace)).toBeUndefined();
              expect(ctx.signal.aborted).toBe(false);
              if (outcome === "Cancelled") {
                expect(() => source.check()).toThrow(
                  expect.objectContaining({ code: "cancelled" }),
                );
                await expect(first.run(source, value)).rejects.toMatchObject({ code: "cancelled" });
              }
              await expect(first.observe(ctx, value)).rejects.toMatchObject({
                reason: "observation_source",
              });
              const temporary = await ctx.get("temporary", "main");
              expect(await temporary.read()).toBe("alice");
              temporary.release();
              // Complete disposal of a visited dependency before continuing the observer (P1).
              await new Promise((resolve) => setTimeout(resolve, 0));
              ctx.check();
              seen.push(value);
            },
          },
        },
      },
    ]);
    const reference = await host.get("execution", "main", { principal: "alice" });
    try {
      const running = reference.run();
      if (outcome === "Cancelled")
        await expect(running).rejects.toMatchObject({ code: "cancelled" });
      else expect(await running).toBe(outcome);
      await finished.promise;
      await Promise.allSettled([...host.session.jobs]);
      if (failure) throw failure;
      expect(seen).toEqual([outcome]);
      expect(() => captured.check()).toThrow();
      await expect(first.observe(source, outcome)).rejects.toMatchObject({ reason: "call" });
    } finally {
      reference.release();
      await host.close();
    }
    expect(host.diagnostics()).toMatchObject({
      roots: 0,
      calls: 0,
      resources: 0,
      values: 0,
      pendingJobs: 0,
    });
  });

test("terminal observation rejects foreign contexts and bound object mismatches without consuming admission", async () => {
  const host = createHost();
  const foreign = createHost();
  let owner!: Hook;
  let wrong!: Hook;
  let count = 0;
  await host.install([
    {
      name: "owner",
      apiVersion: 1,
      hooks: { terminal: series() },
      services: {
        owner: {
          create(self) {
            const hook = self.hook("terminal");
            if (self.key[0] === "right") owner = hook;
            else wrong = hook;
            return { hook };
          },
          methods: {
            async run({ hook }: { hook: Hook }, ctx: Context) {
              await foreign.root(async (other) => {
                await expect(hook.observe(other, null)).rejects.toMatchObject({
                  reason: "context",
                });
              });
              await expect(wrong.observe(ctx, null)).rejects.toMatchObject({
                reason: "operation_owner",
              });
              await hook.observe(ctx, null);
            },
          },
        },
      },
    },
    {
      name: "observer",
      apiVersion: 1,
      requires: { owner: { apiVersion: 1, hooks: ["terminal"] } },
      contributes: {
        owner: {
          terminal: () => {
            count++;
          },
        },
      },
    },
  ]);
  const right = await host.get("owner", "right");
  const other = await host.get("owner", "wrong");
  try {
    await host.root(async (root) => {
      await expect(owner.observe(root, null)).rejects.toMatchObject({
        code: "forbidden",
        reason: "observation_source",
        message: "Hook observation requires the original physical invocation",
      });
    });
    await right.run();
    expect(count).toBe(1);
  } finally {
    right.release();
    other.release();
    await host.close();
    await foreign.close();
  }
});

test("observer errors remain separate and a failed observer still consumes the source admission", async () => {
  const host = createHost();
  const observed = new Error("Observer failed");
  await host.install([
    {
      name: "owner",
      apiVersion: 1,
      hooks: { terminal: series() },
      services: {
        owner: {
          create(self) {
            return { hook: self.hook("terminal") };
          },
          methods: {
            async run({ hook }: { hook: Hook }, ctx: Context) {
              let observerError: unknown;
              try {
                await hook.observe(ctx, "Failed");
              } catch (error) {
                observerError = error;
              }
              await expect(hook.observe(ctx, "Failed")).rejects.toMatchObject({
                reason: "observation_admitted",
              });
              return { outcome: "Failed", observerError: (observerError as Error).message };
            },
          },
        },
      },
    },
    {
      name: "observer",
      apiVersion: 1,
      requires: { owner: { apiVersion: 1, hooks: ["terminal"] } },
      contributes: {
        owner: {
          terminal: () => {
            throw observed;
          },
        },
      },
    },
  ]);
  const reference = await host.get("owner", "main");
  try {
    expect(await reference.run()).toEqual({ outcome: "Failed", observerError: "Observer failed" });
  } finally {
    reference.release();
    await host.close();
  }
  expect(host.diagnostics()).toMatchObject({ roots: 0, calls: 0, resources: 0, pendingJobs: 0 });
});

test("host close cancels observation and awaits its physical callback before releasing owner resources", async () => {
  const host = createHost();
  const entered = deferred<void>();
  const finish = deferred<void>();
  let observation!: Context;
  let resourceClosed = false;
  let physicalFinished = false;
  await host.install([
    {
      name: "owner",
      apiVersion: 1,
      hooks: { terminal: series() },
      services: {
        owner: {
          create(self) {
            self.resource("lease", {
              open: () => ({}),
              close: () => {
                resourceClosed = true;
              },
            });
            return { hook: self.hook("terminal") };
          },
          methods: {
            async run({ hook }: { hook: Hook }, ctx: Context) {
              try {
                await hook.observe(ctx, "Completed");
              } finally {
                physicalFinished = true;
              }
            },
          },
        },
      },
    },
    {
      name: "observer",
      apiVersion: 1,
      requires: { owner: { apiVersion: 1, hooks: ["terminal"] } },
      contributes: {
        owner: {
          terminal: async (ctx) => {
            observation = ctx;
            entered.resolve();
            await finish.promise;
            expect(() => ctx.check()).toThrow(expect.objectContaining({ code: "cancelled" }));
          },
        },
      },
    },
  ]);
  const reference = await host.get("owner", "main");
  const running = reference.run();
  // Observe rejection immediately so cancellation is never an unhandled promise rejection.
  const caller = running.catch((error: unknown) => error);
  await entered.promise;
  let closed = false;
  const closing = host.close().then(() => {
    closed = true;
  });
  try {
    await Promise.resolve();
    expect(observation.signal.aborted).toBe(true);
    expect(resourceClosed).toBe(false);
    expect(physicalFinished).toBe(false);
    expect(closed).toBe(false);
  } finally {
    finish.resolve();
    await caller;
    await closing;
  }
  expect(resourceClosed).toBe(true);
  expect(physicalFinished).toBe(true);
  expect(host.diagnostics()).toMatchObject({ roots: 0, calls: 0, resources: 0, pendingJobs: 0 });
});

test("an old physical owner observes the source version after a compatible replacement", async () => {
  const host = createHost();
  const entered = deferred<void>();
  const proceed = deferred<void>();
  const seen: number[] = [];
  const owner = (version: number) => ({
    name: "owner",
    apiVersion: 1,
    hooks: { terminal: series() },
    services: {
      owner: {
        create(self: Declaration) {
          return { hook: self.hook("terminal") };
        },
        methods: {
          async run({ hook }: { hook: Hook }, ctx: Context) {
            entered.resolve();
            await proceed.promise;
            await hook.observe(ctx, version);
          },
        },
      },
    },
  });
  const observer = (version: number) => ({
    name: "observer",
    apiVersion: 1,
    requires: { owner: { apiVersion: 1, hooks: ["terminal"] } },
    contributes: {
      owner: {
        terminal: (_ctx: Context, value: unknown) => {
          seen.push(version, value as number);
        },
      },
    },
  });
  await host.install([owner(1), observer(1)]);
  const reference = await host.get("owner", "main");
  const running = reference.run();
  await entered.promise;
  try {
    await host.reload([owner(2), observer(2)]);
    proceed.resolve();
    await running;
    expect(seen).toEqual([1, 1]);
    await reference.run();
    expect(seen).toEqual([1, 1, 2, 2]);
  } finally {
    proceed.resolve();
    reference.release();
    await host.close();
  }
});
