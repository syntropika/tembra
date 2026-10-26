import { expect, spyOn, test } from "bun:test";
import { createHost } from "../src/node.ts";
import { orb } from "../src/orb.ts";
import { deferred } from "../src/runtime/records.ts";
import { ActionType, KernelOp } from "../src/protocol.ts";
import type { Context } from "../src/runtime/context.ts";
import type { Host } from "../src/host.ts";
import type { Resource } from "../src/types.ts";

function watchReloadWait(host: Host) {
  const waiting = deferred<void>();
  let program: number | undefined;
  let stopSettled = false;
  const request = host.session.kernel.port.request.bind(host.session.kernel.port);
  const commands = spyOn(host.session.kernel.port, "request").mockImplementation((metadata) => {
    const answer = request(metadata);
    const input = JSON.parse(metadata);
    const output = JSON.parse(answer);
    if (input.op === KernelOp.ScopesReloadRegistered) program = output.ok;
    if (input.op === KernelOp.ProgramCompleteStep && input.program === program) stopSettled = true;
    // The shared Rust program reaches its gate after acknowledging the old scope's Stop effect.
    if (input.op === KernelOp.Drain && stopSettled && output.ok?.length === 0) waiting.resolve();
    return answer;
  });
  return { waiting: waiting.promise, restore: () => commands.mockRestore() };
}

for (const orbOwned of [false, true]) {
  test(`exclusive ${orbOwned ? "orb" : "object"} replacement waits for complete roots and permits their reentry`, async () => {
    const host = createHost();
    const events: string[] = [];
    const entered = deferred<void>();
    const reenter = deferred<void>();
    const reentered = deferred<void>();
    const finish = deferred<void>();
    const releaseLate = deferred<void>();
    const lateContext = deferred<Context>();
    const definition = (version: number) => {
      let orbConnection: Resource<number>;
      const options = {
        version,
        exclusive: true,
        open() {
          events.push(`open:${version}`);
          return version;
        },
        close(value: number) {
          events.push(`close:${value}`);
        },
      };
      return orb({
        name: "root-gate",
        apiVersion: 1,
        create(self) {
          if (orbOwned) orbConnection = self.resource("connection", options);
        },
        services: {
          item: {
            create(self) {
              return {
                connection: orbOwned ? orbConnection : self.resource("connection", options),
              };
            },
            methods: { read: ({ connection }) => connection.value },
          },
        },
      });
    };
    await host.install(definition(1));
    const reference = await host.get("item", "main");
    const observer = watchReloadWait(host);
    const late = host.root(async (ctx) => {
      lateContext.resolve(ctx);
      await releaseLate.promise;
    });
    const admitted = host.root(async (ctx) => {
      expect(await reference.read(ctx)).toBe(1);
      entered.resolve();
      await reenter.promise;
      expect(await reference.read(ctx)).toBe(1);
      reentered.resolve();
      await finish.promise;
    });
    let replacing: Promise<unknown> | undefined;
    try {
      await entered.promise;
      let replaced = false;
      replacing = host.reload([definition(2)]).then(() => {
        replaced = true;
      });
      await observer.waiting;
      expect(replaced).toBe(false);
      expect(events).toEqual(["open:1"]);
      await expect(reference.read(await lateContext.promise)).rejects.toMatchObject({
        reason: "upgrade_conflict",
      });
      reenter.resolve();
      await reentered.promise;
      expect(replaced).toBe(false);
      expect(events).toEqual(["open:1"]);
      finish.resolve();
      await admitted;
      await replacing;
      expect(events).toEqual(["open:1", "close:1", "open:2"]);
      await expect(reference.read(await lateContext.promise)).rejects.toMatchObject({
        reason: "upgrade_conflict",
      });
      expect(await reference.read()).toBe(2);
    } finally {
      reenter.resolve();
      finish.resolve();
      releaseLate.resolve();
      await Promise.allSettled([admitted, late, replacing]);
      observer.restore();
      reference.release();
      await host.close();
    }
    expect(events).toEqual(["open:1", "close:1", "open:2", "close:2"]);
  });
}

test("an exclusive replacement deadline preserves the connection of its blocking root", async () => {
  let now = 0;
  const host = createHost({ now: () => now });
  const events: string[] = [];
  const definition = (version: number) =>
    orb({
      name: "root-deadline",
      apiVersion: 1,
      services: {
        item: {
          create(self) {
            return {
              connection: self.resource("connection", {
                version,
                exclusive: true,
                open() {
                  events.push(`open:${version}`);
                  return version;
                },
                close(value) {
                  events.push(`close:${value}`);
                },
              }),
            };
          },
          methods: { read: ({ connection }) => connection.value },
        },
      },
    });
  await host.install(definition(1));
  const reference = await host.get("item", "main");
  const observer = watchReloadWait(host);
  try {
    await host.root(async (ctx) => {
      expect(await reference.read(ctx)).toBe(1);
      const replacing = host
        .reload([definition(2)], { deadline: 1000 })
        .catch((error: unknown) => error);
      await observer.waiting;
      now = 1000;
      host.session.kernel.command(KernelOp.Tick, { now });
      host.session.lifecycle.flush();
      expect(await replacing).toMatchObject({ reason: "reload_deadline" });
      expect(events).toEqual(["open:1"]);
      expect(await reference.read(ctx)).toBe(1);
    });
    expect(await reference.read()).toBe(1);
  } finally {
    observer.restore();
    reference.release();
    await host.close();
  }
});

test("closing refuses a new replacement before capturing its factory", async () => {
  const host = createHost();
  await host.install(orb({ name: "closing", apiVersion: 1 }));
  let captured = false;
  const closing = host.close();
  await expect(
    host.reload([
      orb({
        name: "closing",
        apiVersion: 1,
        create() {
          captured = true;
        },
      }),
    ]),
  ).rejects.toMatchObject({ code: "closed", reason: "host" });
  await closing;
  expect(captured).toBe(false);
});

test("closing cancels a replacement but waits for its physical opener", async () => {
  const host = createHost();
  const entered = deferred<void>();
  const cancelled = deferred<void>();
  const finish = deferred<void>();
  const closed: number[] = [];
  const started: number[] = [];
  const definition = (version: number) =>
    orb({
      name: "closing-reload",
      apiVersion: 1,
      create(self) {
        self.resource("connection", {
          version,
          async open(signal) {
            if (version === 2) {
              signal.addEventListener("abort", () => cancelled.resolve(), { once: true });
              entered.resolve();
              await finish.promise;
            }
            return version;
          },
          close(value) {
            closed.push(value);
          },
        });
        self.task(() => {
          started.push(version);
        });
      },
    });
  await host.install(definition(1));
  const replacement = host.reload([definition(2)]).then(
    () => undefined,
    (error: unknown) => error,
  );
  await entered.promise;
  let settled = false;
  const closing = host.close().then(() => {
    settled = true;
  });
  await cancelled.promise;
  expect(settled).toBe(false);
  expect(closed).toEqual([]);
  finish.resolve();
  expect(await replacement).toMatchObject({ code: "cancelled", reason: "reload_aborted" });
  await closing;
  expect(closed.sort()).toEqual([1, 2]);
  expect(started).toEqual([1]);
  expect(host.errors()).toEqual([]);
  expect(host.diagnostics()).toMatchObject({ objects: 0, resources: 0, pendingJobs: 0 });
});

test("closing aborts prepared replacements including an empty transaction", async () => {
  for (const empty of [false, true]) {
    const host = createHost();
    const definitions = empty ? [] : [orb({ name: "prepared", apiVersion: 1 })];
    await host.install(definitions);
    const prepared = await host.prepare(definitions);
    await host.close();
    expect(prepared.status).toBe("aborted");
    await prepared.abort();
    await expect(prepared.commit()).rejects.toMatchObject({ reason: "reload_ticket" });
    expect(host.errors()).toEqual([]);
  }
});

test("closing between prepared delivery and ticket publication observes the abort", async () => {
  const host = createHost();
  const definition = orb({ name: "prepared-race", apiVersion: 1 });
  await host.install(definition);
  const entered = deferred<void>();
  const dispatch = host.session.programs.dispatch.bind(host.session.programs);
  let closing: Promise<void> | undefined;
  const delivery = spyOn(host.session.programs, "dispatch").mockImplementation((action) => {
    dispatch(action);
    if (action.type === ActionType.ProgramPrepared && !closing) {
      closing = host.close();
      entered.resolve();
    }
  });
  try {
    const preparing = host.prepare([definition]).catch((error: unknown) => error);
    await entered.promise;
    expect(await preparing).toMatchObject({ reason: "reload_aborted" });
    await closing;
    expect(host.errors()).toEqual([]);
  } finally {
    delivery.mockRestore();
    await host.close();
  }
});

test("closing releases calls blocking an exclusive replacement", async () => {
  const host = createHost();
  const entered = deferred<void>();
  const opened: number[] = [];
  const closed: number[] = [];
  const definition = (version: number) =>
    orb({
      name: "gated-close",
      apiVersion: 1,
      services: {
        item: {
          create(self) {
            self.resource("exclusive", {
              version,
              exclusive: true,
              open() {
                opened.push(version);
                return version;
              },
              close(value) {
                closed.push(value);
              },
            });
          },
          methods: {
            async wait(_self, ctx) {
              await new Promise<void>((resolve) => {
                ctx.signal.addEventListener("abort", () => resolve(), { once: true });
                entered.resolve();
              });
            },
          },
        },
      },
    });
  await host.install(definition(1));
  const reference = await host.get("item", "main");
  const call = reference.wait().catch(() => {});
  await entered.promise;
  const replacement = host.reload([definition(2)]).catch((error: unknown) => error);
  await host.close();
  await call;
  expect(await replacement).toMatchObject({ reason: "reload_aborted" });
  expect(opened).toEqual([1]);
  expect(closed).toEqual([1]);
  expect(host.errors()).toEqual([]);
});

test("closing an exclusive handoff does not reopen the retired resource", async () => {
  const host = createHost();
  const entered = deferred<void>();
  const finish = deferred<void>();
  const cancelled = deferred<void>();
  const opened: number[] = [];
  const closed: number[] = [];
  const definition = (version: number) =>
    orb({
      name: "exclusive-close",
      apiVersion: 1,
      create(self) {
        self.resource("exclusive", {
          version,
          exclusive: true,
          async open(signal) {
            opened.push(version);
            if (version === 2) {
              signal.addEventListener("abort", () => cancelled.resolve(), { once: true });
              entered.resolve();
              await finish.promise;
            }
            return version;
          },
          close(value) {
            closed.push(value);
          },
        });
      },
    });
  await host.install(definition(1));
  const replacement = host.reload([definition(2)]).catch((error: unknown) => error);
  await entered.promise;
  const closing = host.close();
  await cancelled.promise;
  expect(closed).toEqual([1]);
  finish.resolve();
  await closing;
  expect(await replacement).toMatchObject({ reason: "reload_aborted" });
  expect(opened).toEqual([1, 2]);
  expect(closed).toEqual([1, 2]);
  expect(host.errors()).toEqual([]);
});

test("compatible replacement preserves old roots while new roots use new code", async () => {
  const host = createHost();
  const wait = deferred<void>();
  const entered = deferred<void>();
  const definition = (version: number) =>
    orb({
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
    orb({
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
    orb({ name: "new-state", apiVersion: 1, services: { item: { methods: { read: () => 0 } } } }),
  );
  const reference = await host.get("item", "main");
  await host.reload([
    orb({
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
    orb({
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
    orb({
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
    orb({
      name: "ready",
      apiVersion: 1,
      services: { ready: { methods: { version: () => version } } },
    });
  await host.install([
    definition(1),
    orb({
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
    orb({
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
    orb({
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
    orb({
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
    orb({
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
    orb({
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

for (const commit of [false, true])
  test(`migration ${commit ? "adopts" : "discards"} selectors created during the prepared window`, async () => {
    const host = createHost();
    let migrations = 0;
    const definition = (version: number) =>
      orb({
        name: "late-selection",
        apiVersion: 1,
        services: {
          item: {
            create(self) {
              return {
                state: self.state(
                  { count: 1 },
                  {
                    version,
                    migrate(previous: any) {
                      migrations++;
                      return { count: previous.count + 10 };
                    },
                  },
                ),
              };
            },
            methods: { read: ({ state }) => state.value },
          },
        },
      });
    await host.install(definition(1));
    const reference = await host.get("item", "main");
    let computations = 0;
    const early = reference.state.select<number>((value) => {
      computations++;
      return value.count;
    });
    const prepared = await host.prepare([definition(2)]);
    const late = reference.state.select<number>((value) => value.count);
    const child = early.select((value) => value * 2);
    const seen: number[] = [];
    late.subscribe((value) => seen.push(value));
    try {
      expect(late.value).toBe(1);
      if (commit) await prepared.commit();
      else await prepared.abort();
      expect([early.value, late.value, child.value]).toEqual(commit ? [11, 11, 22] : [1, 1, 2]);
      expect(seen).toEqual(commit ? [1, 11] : [1]);
      expect(computations).toBe(commit ? 2 : 1);
      expect(migrations).toBe(1);
    } finally {
      reference.release();
      await host.close();
    }
  });
