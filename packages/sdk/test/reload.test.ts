import { expect, spyOn, test } from "bun:test";
import { createHost } from "../src/node.ts";
import { facet } from "../src/facet.ts";
import { deferred } from "../src/runtime/records.ts";

test("compatible replacement preserves old roots while new roots use new code", async () => {
  const host = createHost();
  const wait = deferred<void>();
  const entered = deferred<void>();
  const definition = (version: number) =>
    facet({
      name: "versioned",
      apiVersion: 1,
      services: {
        versioned: {
          methods: {
            read: () => version,
            async slow() {
              entered.resolve();
              await wait.promise;
              return version;
            },
          },
        },
      },
    });
  await host.install(definition(1));
  const reference = await host.get("versioned", "main");
  const old = reference.slow();
  await entered.promise;
  await host.reload([definition(2)]);
  expect(await reference.read()).toBe(2);
  wait.resolve();
  expect(await old).toBe(1);
  reference.release();
  await host.close();
});

test("exclusive handoff never overlaps and restores old resource after failure", async () => {
  const host = createHost();
  let opened = 0;
  let maximum = 0;
  const definition = (version: number, fail = false) =>
    facet({
      name: "exclusive",
      apiVersion: 1,
      services: {
        exclusive: {
          create(self) {
            const resource = self.resource("connection", {
              version,
              exclusive: true,
              opaque: true,
              open() {
                if (fail) throw new Error("open refused");
                maximum = Math.max(maximum, ++opened);
                return { version };
              },
              close() {
                opened--;
              },
            });
            return { resource };
          },
          methods: { version: ({ resource }) => resource.value.version },
        },
      },
    });
  await host.install(definition(1));
  const reference = await host.get("exclusive", "main");
  await expect(host.reload([definition(2, true)])).rejects.toThrow("open refused");
  expect(await reference.version()).toBe(1);
  expect(reference.opaque<{ version: number }>("connection").version).toBe(1);
  await host.reload([definition(2)]);
  expect(await reference.version()).toBe(2);
  expect(maximum).toBe(1);
  reference.release();
  await host.close();
  expect(opened).toBe(0);
});

test("a replacement can introduce state without exposing the candidate early", async () => {
  const host = createHost();
  await host.install(
    facet({ name: "new-state", apiVersion: 1, services: { item: { methods: { read: () => 0 } } } }),
  );
  const reference = await host.get("item", "main");
  await host.reload([
    facet({
      name: "new-state",
      apiVersion: 1,
      services: {
        item: {
          create(self) {
            return { state: self.state({ value: 42 }) };
          },
          methods: { read: ({ state }) => state.value.value },
        },
      },
    }),
  ]);
  expect(reference.state.value.value).toBe(42);
  expect(await reference.read()).toBe(42);
  reference.release();
  await host.close();
});

test("task replacement waits for physical settlement after cancellation", async () => {
  const host = createHost();
  const finish = deferred<void>();
  const running = deferred<void>();
  const aborted = deferred<void>();
  const starts: number[] = [];
  const definition = (version: number) =>
    facet({
      name: "tasks",
      apiVersion: 1,
      create(self) {
        self.task(async ({ signal }) => {
          starts.push(version);
          running.resolve();
          if (version === 1) {
            signal.addEventListener("abort", () => aborted.resolve(), { once: true });
            await finish.promise;
          }
        });
      },
    });
  await host.install(definition(1));
  await running.promise;
  const replacement = host.reload([definition(2)]);
  await aborted.promise;
  expect(starts).toEqual([1]);
  finish.resolve();
  await replacement;
  expect(starts).toEqual([1, 2]);
  await host.close();
});

test("engine deadlines cancel a physical resource open and restore admission", async () => {
  const host = createHost();
  const definition = (version: number) =>
    facet({
      name: "deadline",
      apiVersion: 1,
      create(self) {
        self.resource("connection", {
          version,
          open: (signal) =>
            version === 1
              ? 1
              : new Promise<number>((_resolve, reject) => {
                  signal.addEventListener("abort", () => reject(signal.reason), { once: true });
                }),
        });
        return {};
      },
      services: { deadline: { methods: { read: () => version } } },
    });
  await host.install(definition(1));
  const reference = await host.get("deadline", "main");
  await expect(host.reload([definition(2)], { deadline: 5 })).rejects.toMatchObject({
    reason: "reload_deadline",
  });
  expect(await reference.read()).toBe(1);
  reference.release();
  await host.close();
});

test("replacement does not wait for an unrelated pending object activation", async () => {
  const host = createHost();
  const opening = deferred<void>();
  const finish = deferred<void>();
  const definition = (version: number) =>
    facet({
      name: "ready",
      apiVersion: 1,
      services: { ready: { methods: { version: () => version } } },
    });
  await host.install([
    definition(1),
    facet({
      name: "unrelated",
      apiVersion: 1,
      services: {
        unrelated: {
          create(self) {
            self.resource("pending", {
              open: async () => {
                opening.resolve();
                await finish.promise;
                return {};
              },
            });
            return {};
          },
          methods: {},
        },
      },
    }),
  ]);
  const reference = await host.get("ready", "main");
  const pending = host.hold("unrelated", "main");
  await opening.promise;
  const replacement = host.reload([definition(2)]);
  let result: string;
  try {
    result = await Promise.race([
      replacement.then(() => "replaced"),
      Bun.sleep(100).then(() => "blocked"),
    ]);
  } finally {
    finish.resolve();
    await pending.ready;
    await replacement;
    pending.release();
  }
  expect(result).toBe("replaced");
  expect(await reference.version()).toBe(2);
  reference.release();
  await host.close();
});

test("replacement refuses an affected pending object activation through the kernel", async () => {
  const host = createHost();
  const opening = deferred<void>();
  const finish = deferred<void>();
  const definition = (version: number) =>
    facet({
      name: "affected",
      apiVersion: 1,
      services: {
        affected: {
          create(self) {
            self.resource("pending", {
              open: async () => {
                opening.resolve();
                await finish.promise;
                return {};
              },
            });
            return {};
          },
          methods: { version: () => version },
        },
      },
    });
  await host.install(definition(1));
  const pending = host.hold("affected", "main");
  await opening.promise;
  const replacement = host.reload([definition(2)]).then(
    () => "replaced",
    (error: unknown) => error,
  );
  let result: unknown;
  try {
    result = await Promise.race([replacement, Bun.sleep(100).then(() => "blocked")]);
  } finally {
    finish.resolve();
    await pending.ready;
    await replacement;
  }
  expect(result).toMatchObject({ reason: "activation_busy" });
  expect(await pending.version()).toBe(1);
  await host.reload([definition(2)]);
  expect(await pending.version()).toBe(2);
  pending.release();
  await host.close();
});

test("replacement reapplies access policies without widening required methods", async () => {
  const host = createHost();
  const definition = (guestMethods: readonly string[]) =>
    facet({
      name: "views",
      apiVersion: 1,
      services: {
        views: {
          access: (principal) => ({
            methods: principal === "guest" ? guestMethods : ["read", "write"],
          }),
          methods: { read: () => "read", write: () => "write" },
        },
      },
    });
  await host.install(definition(["read", "write"]));
  const guest = await host.get("views", "main", { principal: "guest" });
  const required = await host.get("views", "main", { principal: "guest", methods: ["read"] });
  const admin = await host.get("views", "main", { principal: "admin" });
  const savedWrite = guest.write;
  try {
    expect(required.write).toBeUndefined();
    await host.reload([definition(["read"])]);
    expect(guest.write).toBeUndefined();
    expect(required.write).toBeUndefined();
    await expect(savedWrite()).rejects.toMatchObject({ code: "forbidden" });
    expect(await admin.write()).toBe("write");
    await host.reload([definition(["read", "write"])]);
    expect(await guest.write()).toBe("write");
    expect(required.write).toBeUndefined();
    expect(await required.read()).toBe("read");
  } finally {
    guest.release();
    required.release();
    admin.release();
    await host.close();
  }
});

test("replacement updates only affected objects' live reference handles", async () => {
  const host = createHost();
  let affectedChecks = 0;
  let unrelatedChecks = 0;
  const definition = (name: string, version: number) =>
    facet({
      name,
      apiVersion: 1,
      services: {
        [name]: {
          access: () => {
            if (name === "affected") affectedChecks++;
            else unrelatedChecks++;
            return { methods: ["read"] };
          },
          methods: { read: () => version },
        },
      },
    });
  await host.install([definition("affected", 1), definition("unrelated", 1)]);
  const affected = await host.get("affected", "main");
  const unrelated = await host.get("unrelated", "main");
  affectedChecks = unrelatedChecks = 0;
  const traversal = spyOn(host.session.references, "values").mockImplementation(() => {
    throw new Error("Unrelated reference traversal during replacement");
  });
  try {
    await host.reload([definition("affected", 2)]);
    expect(affectedChecks).toBe(1);
    expect(unrelatedChecks).toBe(0);
    expect(await affected.read()).toBe(2);
    expect(await unrelated.read()).toBe(1);
  } finally {
    traversal.mockRestore();
    affected.release();
    unrelated.release();
    await host.close();
  }
});

test("a failing replacement access callback revokes the affected reference view", async () => {
  const host = createHost();
  const definition = (fail: boolean) =>
    facet({
      name: "views",
      apiVersion: 1,
      services: {
        views: {
          access: () => {
            if (fail) throw new Error("Access policy failed");
            return { methods: ["read"] };
          },
          methods: { read: () => 42 },
        },
      },
    });
  await host.install(definition(false));
  const reference = await host.get("views", "main");
  const savedRead = reference.read;
  try {
    await host.reload([definition(true)]);
    expect(reference.read).toBeUndefined();
    await expect(savedRead()).rejects.toMatchObject({ code: "forbidden" });
    expect(host.errors()).toMatchObject([{ message: "Access policy failed" }]);
    await host.reload([definition(false)]);
    expect(await reference.read()).toBe(42);
  } finally {
    reference.release();
    await host.close();
  }
});

test("exclusive handoff closes and restores its nonexclusive resource dependents", async () => {
  const host = createHost();
  const events: string[] = [];
  let connections = 0;
  let watchers = 0;
  let maximumConnections = 0;
  const definition = (version: number, failWatch = false) =>
    facet({
      name: "dependent-handoff",
      apiVersion: 1,
      services: {
        handoff: {
          create(self) {
            const connection = self.resource("connection", {
              version,
              exclusive: true,
              open() {
                events.push(`connection-open:${version}`);
                maximumConnections = Math.max(maximumConnections, ++connections);
                return { version };
              },
              close(value) {
                events.push(`connection-close:${value.version}`);
                connections--;
              },
            });
            const watch = self.resource("watch", {
              deps: [connection],
              open() {
                const value = connection.value;
                if (failWatch) {
                  events.push(`watch-fail:${value.version}`);
                  throw new Error("Watch opener refused");
                }
                events.push(`watch-open:${value.version}`);
                watchers++;
                return { version: value.version };
              },
              close(value) {
                events.push(`watch-close:${value.version}`);
                expect(connection.value.version).toBe(value.version);
                watchers--;
              },
            });
            return { connection, watch };
          },
          methods: {
            read: ({ connection, watch }) => [connection.value.version, watch.value.version],
          },
        },
      },
    });
  await host.install(definition(1));
  const reference = await host.get("handoff", "main");
  const traversal = spyOn(host.session.programs.scopes, "values").mockImplementation(() => {
    throw new Error("Unrelated scope traversal during resource restoration");
  });
  try {
    events.length = 0;
    await host.reload([definition(2)], { deadline: 100 });
    expect(events).toEqual([
      "watch-close:1",
      "connection-close:1",
      "connection-open:2",
      "watch-open:2",
    ]);
    expect(await reference.read()).toEqual([2, 2]);
    expect([connections, watchers, maximumConnections]).toEqual([1, 1, 1]);
    expect(host.session.resources.closedRecords.size).toBe(0);
    events.length = 0;
    await expect(host.reload([definition(3, true)], { deadline: 100 })).rejects.toThrow(
      "Watch opener refused",
    );
    expect(events).toEqual([
      "watch-close:2",
      "connection-close:2",
      "connection-open:3",
      "watch-fail:3",
      "connection-close:3",
      "connection-open:2",
      "watch-open:2",
    ]);
    expect(await reference.read()).toEqual([2, 2]);
    expect([connections, watchers, maximumConnections]).toEqual([1, 1, 1]);
    expect(host.session.resources.closedRecords.size).toBe(0);
  } finally {
    traversal.mockRestore();
    reference.release();
    await host.close();
  }
  expect([connections, watchers]).toEqual([0, 0]);
  expect(host.session.resources.closedRecords.size).toBe(0);
});
