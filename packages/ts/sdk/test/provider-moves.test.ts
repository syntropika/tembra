import { expect, test } from "bun:test";
import { createHost } from "../src/node.ts";
import { Reason } from "../src/identifiers.ts";
import { deferred } from "../src/runtime/records.ts";
import type { Readable } from "../src/state.ts";
import type { FacetDefinition, Resource } from "../src/types.ts";

function provider(
  name: string,
  serves: boolean,
  events: string[] = [],
  linked = false,
  stateVersion = 1,
): FacetDefinition {
  return {
    name,
    apiVersion: 1,
    create(self) {
      return {
        connection: linked
          ? self.resource("parent", { open: () => name, close: () => {} })
          : undefined,
      };
    },
    services: serves
      ? {
          item: {
            create(self) {
              events.push(`factory:${name}`);
              const parent = (self.facet as { connection?: Resource<string> }).connection;
              return {
                state: self.state(0, {
                  version: stateVersion,
                  migrate: (value) => (value as number) + 10,
                }),
                resource: self.resource("owned", {
                  deps: parent ? [parent] : [],
                  open() {
                    events.push(`open:${name}`);
                    return { owner: name };
                  },
                  close() {
                    if (parent) expect(parent.value).toBe(name);
                    events.push(`close:${name}`);
                  },
                }),
              };
            },
            methods: {
              read: ({ state, resource }) => ({
                provider: name,
                value: state.value,
                resource: resource.value,
              }),
              increment: ({ state }, ctx) => state.update(ctx, (value: number) => value + 1),
            },
          },
        }
      : {},
  };
}

for (const abort of [false, true]) {
  for (const linked of [false, true]) {
    test(`service move ${abort ? "abort" : "commit"} preserves state with ${linked ? "facet-dependent" : "reused"} resources`, async () => {
      const host = createHost();
      const events: string[] = [];
      let unrelatedFactories = 0;
      await host.install([
        provider("a", true, events, linked),
        provider("b", false, events, linked),
        {
          name: "unrelated",
          apiVersion: 1,
          services: { other: { create: () => unrelatedFactories++, methods: { read: () => 0 } } },
        },
      ]);
      const reference = await host.get("item", "main");
      const unrelated = await host.get("other", "main");
      const id = host.session.handles.reference(reference).object;
      const state = reference.state as Readable<number>;
      let reacquired;
      try {
        await reference.increment();
        const before = await reference.read();
        const prepared = await host.prepare([
          provider("a", false, events, linked),
          provider("b", true, events, linked),
        ]);
        expect(await reference.read()).toEqual(before);
        if (abort) await prepared.abort();
        else await prepared.commit();
        const after = await reference.read();
        expect(after.provider).toBe(abort ? "a" : "b");
        expect(after.value).toBe(1);
        expect(state.value).toBe(1);
        if (abort || !linked) expect(after.resource).toBe(before.resource);
        else expect(after.resource.owner).toBe("b");
        reacquired = await host.get("item", "main");
        expect(host.session.handles.reference(reacquired).object).toBe(id);
        expect(unrelatedFactories).toBe(1);
        expect(events.filter((event) => event.startsWith("factory:"))).toEqual([
          "factory:a",
          "factory:b",
        ]);
        expect(events.filter((event) => event.startsWith("open:"))).toHaveLength(linked ? 2 : 1);
        await reference.increment();
        expect(state.value).toBe(2);
      } finally {
        reacquired?.release();
        reference.release();
        unrelated.release();
        await host.close();
      }
    });
  }
}

for (const introduce of [false, true]) {
  test(`candidate requirements resolve a ${introduce ? "new" : "previously empty"} destination facet`, async () => {
    const host = createHost();
    const consumer = (facet: string): FacetDefinition => ({
      name: "consumer",
      apiVersion: 1,
      requires: { source: { facet, apiVersion: 1, methods: ["read"] } },
      services: {
        client: {
          create: (self) => ({ source: self.get("source", "main") }),
          methods: { read: ({ source }, ctx) => source.read(ctx) },
        },
      },
    });
    await host.install([
      provider("a", true),
      ...(!introduce ? [provider("b", false)] : []),
      consumer("a"),
    ]);
    const reference = await host.get("item", "main");
    const client = await host.get("client", "main");
    try {
      await reference.increment();
      await host.reload([provider("a", false), provider("b", true), consumer("b")]);
      expect(await client.read()).toMatchObject({ provider: "b", value: 1 });
      expect(await reference.read()).toMatchObject({ provider: "b", value: 1 });
    } finally {
      reference.release();
      client.release();
      await host.close();
    }
  });
}

test("an unchanged requirement pinned to the old facet refuses the move before adoption", async () => {
  const host = createHost();
  await host.install([
    provider("a", true),
    provider("b", false),
    {
      name: "consumer",
      apiVersion: 1,
      requires: { source: { facet: "a", apiVersion: 1, services: { item: ["read"] } } },
    },
  ]);
  const reference = await host.get("item", "main");
  try {
    await reference.increment();
    await expect(host.reload([provider("a", false), provider("b", true)])).rejects.toMatchObject({
      reason: Reason.DependencyService,
    });
    expect(await reference.read()).toMatchObject({ provider: "a", value: 1 });
  } finally {
    reference.release();
    await host.close();
  }
});

test("moving a service can migrate its existing state", async () => {
  const host = createHost();
  await host.install([provider("a", true), provider("b", false)]);
  const reference = await host.get("item", "main");
  try {
    await reference.increment();
    await host.reload([provider("a", false), provider("b", true, [], false, 2)]);
    expect(await reference.read()).toMatchObject({ provider: "b", value: 11 });
  } finally {
    reference.release();
    await host.close();
  }
});

test("a moved service keeps an in-flight root on its previous implementation", async () => {
  const host = createHost();
  const entered = deferred<void>();
  const finish = deferred<void>();
  const definition = (name: string, serves: boolean): FacetDefinition => ({
    name,
    apiVersion: 1,
    services: serves
      ? {
          item: {
            methods: {
              read: () => name,
              async wait() {
                entered.resolve();
                await finish.promise;
                return name;
              },
            },
          },
        }
      : {},
  });
  await host.install([definition("a", true), definition("b", false)]);
  const reference = await host.get("item", "main");
  const old = host.root(async (ctx) => [await reference.wait(ctx), await reference.read(ctx)]);
  try {
    await entered.promise;
    await host.reload([definition("a", false), definition("b", true)]);
    expect(await reference.read()).toBe("b");
    finish.resolve();
    expect(await old).toEqual(["a", "a"]);
  } finally {
    finish.resolve();
    await old;
    reference.release();
    await host.close();
  }
});

test("an incompatible destination refuses a move without losing the existing object", async () => {
  const host = createHost();
  await host.install(provider("a", true));
  const reference = await host.get("item", "main", { methods: ["read", "increment"] });
  try {
    await reference.increment();
    await expect(
      host.reload([
        provider("a", false),
        { name: "b", apiVersion: 1, services: { item: { methods: { write: () => 0 } } } },
      ]),
    ).rejects.toMatchObject({ reason: Reason.MissingMethod });
    expect(await reference.read()).toMatchObject({ provider: "a", value: 1 });
  } finally {
    reference.release();
    await host.close();
  }
});

for (const fail of [false, true]) {
  test(`exclusive resources ${fail ? "recover from a failed" : "never overlap during a"} provider move`, async () => {
    const host = createHost();
    let active = 0;
    let maximum = 0;
    const definition = (name: string, serves: boolean, version: number): FacetDefinition => ({
      name,
      apiVersion: 1,
      services: serves
        ? {
            item: {
              create(self) {
                return {
                  state: self.state(0),
                  resource: self.resource("connection", {
                    version,
                    exclusive: true,
                    open() {
                      if (fail && version === 2) throw new Error("open refused");
                      maximum = Math.max(maximum, ++active);
                      return version;
                    },
                    close() {
                      active--;
                    },
                  }),
                };
              },
              methods: {
                read: ({ state, resource }) => [name, state.value, resource.value],
                increment: ({ state }, ctx) => state.update(ctx, (value: number) => value + 1),
              },
            },
          }
        : {},
    });
    await host.install([definition("a", true, 1), definition("b", false, 1)]);
    const reference = await host.get("item", "main");
    try {
      await reference.increment();
      const moving = host.reload([definition("a", false, 1), definition("b", true, 2)]);
      if (fail) await expect(moving).rejects.toThrow("open refused");
      else await moving;
      expect(await reference.read()).toEqual(fail ? ["a", 1, 1] : ["b", 1, 2]);
      expect(maximum).toBe(1);
    } finally {
      reference.release();
      await host.close();
    }
    expect(active).toBe(0);
  });
}
