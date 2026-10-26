import { expect, test } from "bun:test";
import { createHost } from "../src/node.ts";
import { Code, Reason } from "../src/identifiers.ts";

test("old roots refuse freshly activated objects and retain their existing implementations", async () => {
  const host = createHost();
  const executed: number[] = [];
  const definition = (version: number) => ({
    name: "object-versions",
    apiVersion: 1,
    services: {
      item: {
        create: () => version,
        methods: {
          read(value: number) {
            executed.push(value);
            return value;
          },
        },
      },
    },
  });
  await host.install(definition(1));
  const retained = await host.get("item", "retained");
  try {
    await host.root(async (ctx) => {
      expect(await retained.read(ctx)).toBe(1);
      await host.reload([definition(2)]);
      const fresh = await ctx.get("item", "fresh");
      try {
        await expect(fresh.read(ctx)).rejects.toMatchObject({
          code: Code.Conflict,
          reason: Reason.UpgradeConflict,
        });
        expect(executed).toEqual([1]);
        expect(host.session.lifecycle.diagnostics().calls).toBe(0);
        expect(await retained.read(ctx)).toBe(1);
        expect(await host.root((current) => fresh.read(current))).toBe(2);
        expect(await retained.read()).toBe(2);
        expect(executed).toEqual([1, 1, 2, 2]);
      } finally {
        fresh.release();
      }
    });
  } finally {
    retained.release();
    await host.close();
  }
  expect(host.session.lifecycle.diagnostics().calls).toBe(0);
  expect(host.session.lifecycle.diagnostics().objects).toBe(0);
});
