import { expect, spyOn, test } from "bun:test";
import { createHost, ContextKey, opaqueFrom } from "../src/node.ts";
import { KernelOp } from "../src/protocol.ts";
import { deferred } from "../src/runtime/records.ts";
import { bail } from "../src/index.ts";

const Trace = ContextKey.text("test/trace");
const stats = (host: ReturnType<typeof createHost>) =>
  host.session.kernel.command<{ live: number; peak: number }>(KernelOp.ContextStats);

test("immutable concurrent branches propagate independently per admitted call", async () => {
  const host = createHost();
  const entered = deferred<void>();
  const finish = deferred<void>();
  let active = 0;
  let captured: import("../src/runtime/context.ts").Context | undefined;
  await host.install({
    name: "branches",
    apiVersion: 1,
    services: {
      echo: {
        methods: {
          async read(_value: unknown, ctx: import("../src/runtime/context.ts").Context) {
            captured = ctx.with(Trace, "inside");
            if (++active === 2) entered.resolve();
            await finish.promise;
            return [ctx.value(Trace), ctx.principal];
          },
        },
      },
    },
  });
  const reference = await host.get("echo", "main");
  try {
    await host.root(async (ctx) => {
      const left = ctx.with(Trace, "left");
      const right = ctx.with(Trace, "right");
      expect(ctx.value(Trace)).toBe(undefined);
      expect(ctx.value(ContextKey.integer("test/trace"))).toBe(undefined);
      const Integer = ContextKey.integer("test/integer");
      const Boolean = ContextKey.boolean("test/boolean");
      const scalar = ctx.with(Integer, Number.MAX_SAFE_INTEGER).with(Boolean, true);
      expect(scalar.value(Integer)).toBe(Number.MAX_SAFE_INTEGER);
      expect(scalar.value(Boolean)).toBe(true);
      const calls = [reference.read(left), reference.read(right)];
      await entered.promise;
      expect(stats(host).live).toBe(6);
      finish.resolve();
      expect(await Promise.all(calls)).toEqual([
        ["left", "host"],
        ["right", "host"],
      ]);
      expect(left.value(Trace)).toBe("left");
      expect(() => captured!.with(Trace, "expired")).toThrow();
      expect(() => ctx.with(ContextKey.integer("test/trace"), 1)).toThrow();
      expect(() => ctx.with(ContextKey.integer("test/unsafe"), 2 ** 53)).toThrow();
      const other = createHost({ principal: "receiver" });
      try {
        await other.root(async (foreign) => {
          await expect(Promise.resolve().then(() => reference.read(foreign))).rejects.toMatchObject(
            { reason: "context" },
          );
          const metadata = JSON.parse(JSON.stringify(left.exportValues([Trace])));
          const imported = foreign.with(Trace, metadata["test/trace@1"]);
          expect(imported.value(Trace)).toBe("left");
          expect(imported.principal).toBe("receiver");
          expect(foreign.value(Trace)).toBe(undefined);
        });
      } finally {
        await other.close();
      }
      expect(ctx.with(Trace, "portable").exportValues([Trace])).toEqual({
        "test/trace@1": "portable",
      });
      expect(() => ctx.exportValues([ContextKey.opaque("test/scope")])).toThrow();
      expect(() => ctx.with(ContextKey.opaque("test/scope"), { opaque: true })).toThrow();
      expect(() => opaqueFrom(ctx, ContextKey.opaque("test/scope"), left.frame)).toThrow();
    });
    expect(stats(host).live).toBe(0);
  } finally {
    finish.resolve();
    reference.release();
    await host.close();
  }
});

test("hook contributions inherit each caller's derived frame", async () => {
  const host = createHost();
  await host.install([
    {
      name: "context-hooks",
      apiVersion: 1,
      hooks: { inspect: bail() },
      services: {
        "context-hooks": {
          create(self) {
            return { inspect: self.hook("inspect") };
          },
          methods: {
            read({ inspect }, ctx) {
              return inspect.run(ctx, null);
            },
          },
        },
      },
    },
    {
      name: "context-reader",
      apiVersion: 1,
      requires: { "context-hooks": { apiVersion: 1, hooks: ["inspect"] } },
      contributes: { "context-hooks": { inspect: (ctx) => ctx.value(Trace) } },
    },
  ]);
  const reference = await host.get("context-hooks", "main");
  try {
    await host.root(async (root) => {
      const values = await Promise.all([
        reference.read(root.with(Trace, "left")),
        reference.read(root.with(Trace, "right")),
      ]);
      expect(values).toEqual(["left", "right"]);
      expect(root.value(Trace)).toBeUndefined();
    });
    expect(stats(host).live).toBe(0);
  } finally {
    reference.release();
    await host.close();
  }
});

test("propagating a reused frame adds no per-call bridge requests", async () => {
  const host = createHost();
  await host.install({
    name: "context-cost",
    apiVersion: 1,
    services: { "context-cost": { methods: { read: () => 42 } } },
  });
  const reference = await host.get("context-cost", "main");
  const commands = spyOn(host.session.kernel, "command");
  try {
    await host.root(async (root) => {
      const tagged = root.with(Trace, "reused");
      await reference.read(root);
      commands.mockClear();
      await reference.read(root);
      const plain = commands.mock.calls.map(([op]) => op);
      commands.mockClear();
      await reference.read(tagged);
      expect(commands.mock.calls.map(([op]) => op)).toEqual(plain);
      const entered = commands.mock.calls.find(([op]) => op === KernelOp.BeginCall);
      expect(entered?.[1]?.context).toBe(tagged.frame);
    });
  } finally {
    commands.mockRestore();
    reference.release();
    await host.close();
  }
});
