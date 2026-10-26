import { expect, spyOn, test } from "bun:test";
import { createHost } from "../src/node.ts";
import { facet } from "../src/facet.ts";
import { Code, Reason } from "../src/identifiers.ts";
import { KernelOp } from "../src/protocol.ts";
import { Errors } from "../src/errors.ts";

for (const alias of [false, true])
  test(`a facet factory rejects its own service${alias ? " through an alias" : ""} before acquisition`, async () => {
    const host = createHost();
    let opens = 0;
    const commands = spyOn(host.session.kernel, "command");
    try {
      const installing = host.install(
        facet({
          name: "self",
          apiVersion: 1,
          ...(alias
            ? { requires: { own: { facet: "self", apiVersion: 1, methods: ["read"] } } }
            : {}),
          create(self) {
            self.resource("connection", {
              open: () => {
                opens++;
                return 1;
              },
            });
            return self.get(alias ? "own" : "self", "main");
          },
          services: { self: { methods: { read: () => 1 } } },
        }),
      );
      const outcome = await Promise.race([
        installing.then(
          () => null,
          (error) => error,
        ),
        new Promise((resolve) => setImmediate(() => resolve("pending"))),
      ]);
      expect(outcome).toMatchObject({ code: Code.Invalid, reason: Reason.DependencyCycle });
      expect(commands.mock.calls.filter(([op]) => op === KernelOp.Acquire)).toEqual([]);
      expect(opens).toBe(0);
      expect(host.diagnostics()).toMatchObject({ objects: 0, resources: 0, calls: 0 });
      await host.reload([
        facet({ name: "self", apiVersion: 1, services: { self: { methods: { read: () => 42 } } } }),
      ]);
      const reference = await host.get("self", "main");
      expect(await reference.read()).toBe(42);
      reference.release();
    } finally {
      commands.mockRestore();
      await host.close();
    }
    expect(host.diagnostics()).toMatchObject({ objects: 0, resources: 0, pendingJobs: 0 });
  });

test("rejecting a self-dependent reload preserves the live facet and permits a corrected reload", async () => {
  const host = createHost();
  const definition = (version: number) =>
    facet({
      name: "self",
      apiVersion: 1,
      create: () => ({ version }),
      services: {
        self: { create: (self) => self.facet, methods: { read: (value) => value.version } },
      },
    });
  await host.install(definition(1));
  const reference = await host.get("self", "main");
  const version = host.installedFacets()[0]!.codeVersion;
  try {
    await expect(
      host.reload([
        facet({
          ...definition(2),
          requires: { own: { facet: "self", apiVersion: 1, methods: ["read"] } },
          create: (self) => self.get("own", "main"),
        }),
      ]),
    ).rejects.toMatchObject(Errors.Invalid.DependencyCycle);
    expect(host.installedFacets()[0]!.codeVersion).toBe(version);
    expect(await reference.read()).toBe(1);
    await host.reload([definition(2)]);
    expect(await reference.read()).toBe(2);
  } finally {
    reference.release();
    await host.close();
  }
  expect(host.diagnostics()).toMatchObject({ objects: 0, resources: 0, pendingJobs: 0 });
});

test("facet factories may use external services and object factories may use sibling services", async () => {
  const host = createHost();
  await host.install(
    facet({
      name: "provider",
      apiVersion: 1,
      services: { remote: { methods: { read: () => 42 } } },
    }),
  );
  await host.install(
    facet({
      name: "consumer",
      apiVersion: 1,
      requires: { external: { facet: "provider", apiVersion: 1, methods: ["read"] } },
      create: (self) => ({ external: self.get("external", "main") }),
      services: {
        local: { methods: { read: () => 7 } },
        reader: {
          create: (self) => ({ facet: self.facet, local: self.get("local", "main") }),
          methods: {
            read: async ({ facet, local }, ctx) => [
              await local.read(ctx),
              await facet.external.read(ctx),
            ],
          },
        },
      },
    }),
  );
  const reference = await host.get("reader", "main");
  try {
    expect(await reference.read()).toEqual([7, 42]);
  } finally {
    reference.release();
    await host.close();
  }
  expect(host.diagnostics()).toMatchObject({ objects: 0, resources: 0, pendingJobs: 0 });
});
