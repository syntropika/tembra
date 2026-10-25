import { expect, spyOn, test } from "bun:test";
import { createHost } from "../src/node.ts";
import { facet } from "../src/facet.ts";
import type { Reload } from "../src/kernel.ts";

test("resource dependencies reject foreign handles even when host-local IDs collide", async () => {
  const first = createHost({ resources: { base: { open: () => "first" } } });
  const second = createHost({ resources: { base: { open: () => "second" } } });
  const foreign = first.session.hostResource("base");
  const local = second.session.hostResource("base");
  const foreignRecord = first.session.resources.records.values().next().value!;
  const localRecord = second.session.resources.records.values().next().value!;
  expect(foreignRecord.id).toBe(localRecord.id);
  await first.install(facet({ name: "first", apiVersion: 1 }));
  await second.install(
    facet({
      name: "second",
      apiVersion: 1,
      services: {
        foreign: {
          create(self) {
            self.resource("dependent", { deps: [foreign], open: () => null });
            return {};
          },
          methods: {},
        },
        local: {
          create(self) {
            self.resource("dependent", { deps: [local], open: () => null });
            return {};
          },
          methods: {},
        },
      },
    }),
  );
  const before = second.diagnostics().resources;
  try {
    await expect(second.get("foreign", "main")).rejects.toThrow("same host");
    expect(second.diagnostics().resources).toBe(before);
    const reference = await second.get("local", "main");
    reference.release();
  } finally {
    await Promise.all([first.close(), second.close()]);
  }
});

test("acquiring JS services does not request unrelated installed metadata", async () => {
  const host = createHost();
  await host.install(
    facet({ name: "local", apiVersion: 1, services: { local: { methods: { read: () => 42 } } } }),
  );
  const commands = spyOn(host.session.kernel, "command");
  try {
    const first = await host.get("local", "first");
    const second = await host.get("local", "second");
    expect(await first.read()).toBe(42);
    expect(commands.mock.calls.filter(([op]) => op === "facets")).toHaveLength(0);
    first.release();
    second.release();
  } finally {
    commands.mockRestore();
    await host.close();
  }
});

test("native provider metadata is looked up once by acquired facet identity", async () => {
  let facetId = 0;
  const host = createHost({
    native: {
      hasFacet: (id) => id === facetId,
      activate: async () => {},
      invoke: async (_invocation, arguments_) => arguments_[0],
      dispose: async () => {},
      effect: async () => {},
      value: () => {
        throw new Error("This provider has no portable state");
      },
    },
  });
  const prepared = host.session.kernel.command<Reload>("prepare", {
    facets: [
      {
        name: "native",
        apiVersion: 1,
        services: [{ name: "native", methods: ["echo"] }],
      },
    ],
  });
  facetId = prepared.facets[0]!.id;
  host.session.kernel.command("commit", { ticket: prepared.id });
  const commands = spyOn(host.session.kernel, "command");
  try {
    const first = await host.get("native", "first");
    const second = await host.get("native", "second");
    expect(await first.echo(42)).toBe(42);
    expect(await second.echo(43)).toBe(43);
    expect(commands.mock.calls.filter(([op]) => op === "facets")).toHaveLength(0);
    expect(commands.mock.calls.filter(([op]) => op === "facet")).toEqual([
      ["facet", { id: facetId }],
    ]);
    first.release();
    second.release();
  } finally {
    commands.mockRestore();
    await host.close();
  }
});

test("disposing one object does not traverse unrelated JS reference records", async () => {
  const host = createHost();
  await host.install(
    facet({ name: "local", apiVersion: 1, services: { local: { methods: { read: () => 42 } } } }),
  );
  const first = await host.get("local", "first");
  const second = await host.get("local", "second");
  const object = host.session.reference(first).object;
  const traversal = spyOn(host.session.references, "values").mockImplementation(() => {
    throw new Error("Unrelated reference traversal during object disposal");
  });
  try {
    first.release();
    await host.session.objects.get(object)!.disposing;
    expect(host.session.references.size).toBe(1);
    expect(await second.read()).toBe(42);
  } finally {
    traversal.mockRestore();
    second.release();
    await host.close();
  }
});

test("declared aliases resolve the current provider without listing unrelated facets", async () => {
  const host = createHost();
  const provider = (version: number) =>
    facet({
      name: "provider",
      apiVersion: 1,
      services: { storage: { methods: { read: () => version } } },
    });
  await host.install([
    provider(1),
    facet({
      name: "consumer",
      apiVersion: 1,
      requires: { store: { facet: "provider", apiVersion: 1, methods: ["read"] } },
      services: {
        consumer: {
          create(self) {
            return { storage: self.get("store", "main") };
          },
          methods: { read: ({ storage }, ctx) => storage.read(ctx) },
        },
      },
    }),
    facet({ name: "unrelated", apiVersion: 1 }),
  ]);
  let commands = spyOn(host.session.kernel, "command");
  try {
    const first = await host.get("consumer", "first");
    expect(await first.read()).toBe(1);
    expect(commands.mock.calls.filter(([op]) => op === "facets")).toHaveLength(0);
    expect(commands.mock.calls.filter(([op]) => op === "facet-named")).toEqual([
      ["facet-named", { name: "provider" }],
    ]);
    commands.mockRestore();
    await host.reload([provider(2)]);
    commands = spyOn(host.session.kernel, "command");
    const second = await host.get("consumer", "second");
    expect(await first.read()).toBe(2);
    expect(await second.read()).toBe(2);
    expect(commands.mock.calls.filter(([op]) => op === "facets")).toHaveLength(0);
    expect(commands.mock.calls.filter(([op]) => op === "facet-named")).toEqual([
      ["facet-named", { name: "provider" }],
    ]);
    first.release();
    second.release();
  } finally {
    commands.mockRestore();
    await host.close();
  }
});

test("local references do not expose inherited object members as service methods", async () => {
  const host = createHost();
  await host.install(
    facet({ name: "local", apiVersion: 1, services: { local: { methods: { read: () => 42 } } } }),
  );
  const reference = await host.get("local", "main");
  try {
    for (const method of ["constructor", "hasOwnProperty", "valueOf", "toString"])
      expect(reference[method]).toBeUndefined();
    expect(await reference.read()).toBe(42);
  } finally {
    reference.release();
    await host.close();
  }
});
