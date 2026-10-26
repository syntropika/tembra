import { expect, test } from "bun:test";
import { createHost } from "../src/node.ts";
import { deferred } from "../src/runtime/records.ts";
import type { Declaration } from "../src/runtime/declaration.ts";
import type { OrbDefinition } from "../src/types.ts";

for (const orbOwned of [false, true]) {
  for (const abort of [false, true]) {
    test(`reused ${orbOwned ? "orb" : "object"} resource callbacks ${abort ? "remain unchanged on abort" : "adopt live dependencies on commit"}`, async () => {
      const host = createHost();
      const events: string[] = [];
      const connection = (self: Declaration, generation: number) => {
        const store = self.get("store", "main");
        return self.resource("connection", {
          deps: [store],
          open() {
            events.push(`open:${generation}`);
            return 1;
          },
          async close(value, ctx) {
            events.push(`close:${generation}:${value}:${await store.read(ctx)}`);
          },
        });
      };
      const definition = (generation: number): OrbDefinition => ({
        name: "consumer",
        apiVersion: 1,
        requires: { store: { orb: "storage", apiVersion: 1, methods: ["read"] } },
        create(self) {
          return orbOwned ? { resource: connection(self, generation) } : {};
        },
        services: {
          item: {
            create(self) {
              return orbOwned ? self.orb : { resource: connection(self, generation) };
            },
            methods: { read: ({ resource }) => resource.value },
          },
        },
      });
      await host.install([
        { name: "storage", apiVersion: 1, services: { store: { methods: { read: () => 42 } } } },
        definition(1),
      ]);
      const reference = await host.get("item", "main");
      try {
        const prepared = await host.prepare([definition(2)]);
        expect(events).toEqual(["open:1"]);
        expect(await reference.read()).toBe(1);
        if (abort) await prepared.abort();
        else await prepared.commit();
        expect(await reference.read()).toBe(1);
        expect(events).toEqual(["open:1"]);
      } finally {
        reference.release();
        await host.close();
      }
      expect(events).toEqual(["open:1", `close:${abort ? 1 : 2}:1:42`]);
    });
  }
}

test("failed exclusive replacement restores the last committed resource callbacks", async () => {
  const host = createHost();
  const events: string[] = [];
  const definition = (generation: number, version = 1, fail = false): OrbDefinition => ({
    name: "exclusive",
    apiVersion: 1,
    services: {
      item: {
        create(self) {
          return {
            resource: self.resource("connection", {
              version,
              exclusive: true,
              open() {
                events.push(`open:${generation}`);
                if (fail) throw new Error("open refused");
                return generation;
              },
              close(value) {
                events.push(`close:${generation}:${value}`);
              },
            }),
          };
        },
        methods: { read: ({ resource }) => resource.value },
      },
    },
  });
  await host.install(definition(1));
  const reference = await host.get("item", "main");
  try {
    await host.reload([definition(2)]);
    expect(await reference.read()).toBe(1);
    await expect(host.reload([definition(3, 2, true)])).rejects.toThrow("open refused");
    expect(await reference.read()).toBe(2);
    expect(events).toEqual(["open:1", "close:2:1", "open:3", "open:2"]);
  } finally {
    reference.release();
    await host.close();
  }
  expect(events.at(-1)).toBe("close:2:2");
});

test("compatible resource adoption preserves the dependencies of an in-flight old implementation", async () => {
  const host = createHost();
  const entered = deferred<void>();
  const finish = deferred<void>();
  const closed: number[] = [];
  const definition = (generation: number): OrbDefinition => ({
    name: "consumer",
    apiVersion: 1,
    requires: { store: { orb: "storage", apiVersion: 1, methods: ["read"] } },
    services: {
      item: {
        create(self) {
          const store = self.get("store", "main");
          self.resource("connection", {
            deps: [store],
            open: () => null,
            async close(_value, ctx) {
              expect(await store.read(ctx)).toBe(42);
              closed.push(generation);
            },
          });
          return { store, generation };
        },
        methods: {
          async wait({ store, generation }, ctx) {
            entered.resolve();
            await finish.promise;
            return [generation, await store.read(ctx)];
          },
          read: ({ generation }) => generation,
        },
      },
    },
  });
  await host.install([
    { name: "storage", apiVersion: 1, services: { store: { methods: { read: () => 42 } } } },
    definition(1),
  ]);
  const reference = await host.get("item", "main");
  const old = reference.wait();
  try {
    await entered.promise;
    await host.reload([definition(2)]);
    expect(await reference.read()).toBe(2);
    finish.resolve();
    expect(await old).toEqual([1, 42]);
  } finally {
    finish.resolve();
    await old;
    reference.release();
    await host.close();
  }
  expect(closed).toEqual([2]);
});
