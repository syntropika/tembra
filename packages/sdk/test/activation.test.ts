import { EffectType, KernelOp, OwnerKind } from "../src/protocol.ts";
import { expect, test } from "bun:test";
import { createHost } from "../src/node.ts";
import { facet } from "../src/facet.ts";
import { deferred } from "../src/runtime/records.ts";
import type { Reload } from "../src/kernel.ts";

test("concurrent facets share physical host resource activation", async () => {
  const opened = deferred<void>();
  const finish = deferred<void>();
  let attempts = 0;
  let closes = 0;
  const host = createHost({
    resources: {
      shared: {
        open: async () => {
          attempts++;
          opened.resolve();
          await finish.promise;
          return 42;
        },
        close: () => {
          closes++;
        },
      },
    },
  });
  const installing = host.install([
    facet({ name: "first", apiVersion: 1 }),
    facet({ name: "second", apiVersion: 1 }),
  ]);
  await opened.promise;
  expect(attempts).toBe(1);
  finish.resolve();
  await installing;
  await host.close();
  expect(closes).toBe(1);
});

test("a facet resource opener can fail once and recover on acquisition", async () => {
  let attempts = 0;
  let closes = 0;
  const host = createHost();
  const definition = facet({
    name: "retry",
    apiVersion: 1,
    create(self) {
      return {
        resource: self.resource("connection", {
          open: () => {
            if (++attempts === 1) throw new Error("Temporary facet failure");
            return 42;
          },
          close: () => {
            closes++;
          },
        }),
      };
    },
    services: {
      retry: { create: (self) => self.facet, methods: { read: (value) => value.resource.value } },
    },
  });
  try {
    await expect(host.install(definition)).rejects.toThrow("Temporary facet failure");
    const reference = await host.get("retry", "main");
    expect(await reference.read()).toBe(42);
    reference.release();
    expect(attempts).toBe(2);
  } finally {
    await host.close();
  }
  expect(closes).toBe(1);
});

test("a throwing facet factory cleans its declarations and can retry", async () => {
  let attempts = 0;
  let opens = 0;
  let closes = 0;
  const host = createHost();
  const definition = facet({
    name: "factory",
    apiVersion: 1,
    create(self) {
      self.resource("connection", {
        open: () => ++opens,
        close: () => {
          closes++;
        },
      });
      if (++attempts === 1) throw new Error("Temporary factory failure");
      return {};
    },
    services: { factory: { methods: { read: () => 42 } } },
  });
  try {
    await expect(host.install(definition)).rejects.toThrow("Temporary factory failure");
    expect(host.diagnostics().resources).toBe(0);
    const reference = await host.get("factory", "main");
    expect(await reference.read()).toBe(42);
    reference.release();
  } finally {
    await host.close();
  }
  expect([attempts, opens, closes]).toEqual([2, 1, 1]);
});

test("host resource retry preserves issued handles and dependent resource metadata", async () => {
  let attempts = 0;
  let closes = 0;
  const host = createHost({
    resources: {
      connection: {
        open: () => {
          if (++attempts === 1) throw new Error("Temporary host failure");
          return 42;
        },
        close: () => {
          closes++;
        },
      },
    },
  });
  const original = host.session.hostResource<number>("connection");
  const definition = facet({
    name: "consumer",
    apiVersion: 1,
    services: {
      consumer: {
        create(self) {
          const connection = self.resource<number>("connection");
          expect(connection).toBe(original);
          const watch = self.resource("watch", {
            deps: [connection],
            open: () => connection.value + 1,
          });
          return { watch };
        },
        methods: { read: ({ watch }) => watch.value },
      },
    },
  });
  try {
    await expect(host.install(definition)).rejects.toThrow("Temporary host failure");
    const reference = await host.get("consumer", "main");
    expect(await reference.read()).toBe(43);
    expect(original.value).toBe(42);
    reference.release();
  } finally {
    await host.close();
  }
  expect([attempts, closes]).toEqual([2, 1]);
});

test("dynamic host resources open and remain available to object closers during shutdown", async () => {
  const host = createHost();
  const opened = deferred<void>();
  const events: string[] = [];
  const connection = host.resource("dynamic", {
    open: () => {
      opened.resolve();
      return 42;
    },
    close: () => {
      events.push("host");
    },
  });
  await host.install(
    facet({
      name: "consumer",
      apiVersion: 1,
      services: {
        consumer: {
          create(self) {
            self.resource("owned", {
              open: () => ({}),
              close: () => {
                expect(connection.value).toBe(42);
                events.push("object");
              },
            });
            return {};
          },
          methods: {},
        },
      },
    }),
  );
  await opened.promise;
  const reference = await host.get("consumer", "main");
  await host.close();
  reference.release();
  expect(events).toEqual(["object", "host"]);
  expect(host.diagnostics().resources).toBe(0);
});

test("retry refreshes a pending dynamic resource's dependency IDs after a failed host opener", async () => {
  let attempts = 0;
  const host = createHost({
    resources: {
      base: {
        open: () => {
          if (++attempts === 1) throw new Error("Temporary base failure");
          return 41;
        },
      },
    },
  });
  const base = host.session.hostResource<number>("base");
  const dependent = host.resource("dependent", { deps: [base], open: () => base.value + 1 });
  try {
    await expect(host.install(facet({ name: "first", apiVersion: 1 }))).rejects.toThrow(
      "Temporary base failure",
    );
    await host.install(facet({ name: "second", apiVersion: 1 }));
    expect(dependent.value).toBe(42);
    expect(attempts).toBe(2);
  } finally {
    await host.close();
  }
  expect(host.diagnostics().resources).toBe(0);
});

test("a failed dynamic resource leaves live host resources usable and retries its issued handle", async () => {
  let attempts = 0;
  let closes = 0;
  const host = createHost({ resources: { base: { open: () => 41 } } });
  await host.install(
    facet({ name: "live", apiVersion: 1, services: { live: { methods: { read: () => 7 } } } }),
  );
  const reference = await host.get("live", "main");
  const base = host.session.hostResource<number>("base");
  const dynamic = host.resource("dynamic", {
    deps: [base],
    open: () => {
      if (++attempts === 1) throw new Error("Temporary dynamic failure");
      return base.value + 1;
    },
    close: () => {
      closes++;
    },
  });
  try {
    await Promise.allSettled([...host.session.jobs]);
    expect(await reference.read()).toBe(7);
    expect(base.value).toBe(41);
    await host.install(facet({ name: "retry", apiVersion: 1 }));
    expect(host.session.hostResource("dynamic")).toBe(dynamic);
    expect(dynamic.value).toBe(42);
    expect(attempts).toBe(2);
  } finally {
    reference.release();
    await host.close();
  }
  expect(closes).toBe(1);
});

test("native opener cancellation signals the provider and retains its physical future", async () => {
  const opened = deferred<void>();
  const cancelled = deferred<void>();
  const finish = deferred<void>();
  let facetId = 0;
  let cancellation = false;
  let settled = false;
  const host = createHost({
    native: {
      hasFacet: (id) => id === facetId,
      activate: async (object) => {
        const resource = host.session.kernel.command<{ id: number }>(KernelOp.Resource, {
          owner: { kind: OwnerKind.Object, id: object },
          name: "native",
          version: 1,
          dependencies: [],
        });
        host.session.kernel.command(KernelOp.ScopeRegister, {
          scope: host.session.kernel.command<number>(KernelOp.Allocate),
          owner: { kind: OwnerKind.Object, id: object },
          facet: facetId,
          parents: [],
          dependencies: [],
          resources: [resource.id],
          migrate: false,
        });
      },
      invoke: async () => undefined,
      dispose: async () => {},
      effect: async (effect) => {
        if (effect.type === EffectType.Open) {
          opened.resolve();
          await finish.promise;
          settled = true;
          if (cancellation) throw new Error("Native opener cancelled");
        } else if (effect.type === EffectType.Close) {
          host.session.kernel.command(KernelOp.ResourceClosed, { resource: effect.resource });
        }
      },
      cancelEffect: async () => {
        cancellation = true;
        cancelled.resolve();
      },
      value: () => {
        throw new Error("No portable state");
      },
    },
  });
  const prepared = host.session.kernel.command<Reload>(KernelOp.Prepare, {
    facets: [{ name: "native", apiVersion: 1, services: [{ name: "native", methods: [] }] }],
  });
  facetId = prepared.facets[0]!.id;
  host.session.kernel.command(KernelOp.Commit, { ticket: prepared.id });
  const getting = host.get("native", "main");
  getting.catch(() => {});
  await opened.promise;
  let closed = false;
  const closing = host.close().then(() => {
    closed = true;
  });
  closing.catch(() => {});
  try {
    await cancelled.promise;
    expect(settled).toBe(false);
    expect(closed).toBe(false);
  } finally {
    finish.resolve();
  }
  await expect(getting).rejects.toThrow();
  await closing;
  expect(settled).toBe(true);
  expect(host.diagnostics().resources).toBe(0);
});
