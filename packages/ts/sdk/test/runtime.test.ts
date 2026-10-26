import { describe, expect, test } from "bun:test";
import { createHost } from "../src/node.ts";
import { orb } from "../src/orb.ts";
import { waterfall } from "../src/hooks.ts";
import { deferred } from "../src/runtime/records.ts";

function counter(version = 1, release?: () => void) {
  return orb({
    name: "counter",
    apiVersion: 1,
    services: {
      counter: {
        create(self) {
          const state = self.state(
            { value: 0 },
            {
              version,
              validate: (next) => Number.isSafeInteger(next.value),
              migrate: (old) => ({ value: (old as { value: number }).value + 10 }),
            },
          );
          self.resource("connection", { open: () => ({}), close: () => release?.() });
          return { state };
        },
        methods: {
          increment({ state }, ctx) {
            state.update(ctx, (s: { value: number }) => ({ value: s.value + 1 }));
          },
          set({ state }, ctx, value: number) {
            state.update(ctx, () => ({ value }));
          },
        },
      },
    },
  });
}

describe("minimal orb runtime on the real Rust Node binding", () => {
  test("state stays immutable and local reads retain identity", async () => {
    const host = createHost();
    await host.install(counter());
    const ref = await host.get("counter", "main");
    const before = ref.state.value;
    expect(ref.state.value).toBe(before);
    expect(Object.isFrozen(before)).toBe(true);
    await ref.increment();
    expect(ref.state.value.value).toBe(1);
    expect(before.value).toBe(0);
    await expect(ref.set(1.5)).rejects.toMatchObject({ reason: "state_validation" });
    expect(ref.state.value.value).toBe(1);
    ref.release();
    await host.close();
    expect(host.diagnostics().objects).toBe(0);
    expect(host.diagnostics().values).toBe(0);
  });

  test("selectors use identity and publish after adoption", async () => {
    const host = createHost();
    await host.install(counter());
    const ref = await host.get("counter", "main");
    const parity = ref.state.select((state) => state.value % 2);
    const seen: number[] = [];
    const detach = parity.subscribe((value: number) => seen.push(value));
    await ref.set(2);
    expect(parity.value).toBe(0);
    expect(seen).toEqual([0]);
    await ref.increment();
    expect(parity.value).toBe(1);
    expect(seen).toEqual([0, 1]);
    detach();
    ref.release();
    await host.close();
  });

  test("compatible reload preserves resource and state; migration advances state", async () => {
    let closed = 0;
    const host = createHost();
    await host.install(counter(1, () => closed++));
    const ref = await host.get("counter", "main");
    await ref.increment();
    await host.reload([counter(1, () => closed++)]);
    expect(closed).toBe(0);
    expect(ref.state.value.value).toBe(1);
    await host.reload([counter(2, () => closed++)]);
    expect(ref.state.value.value).toBe(11);
    ref.release();
    await host.close();
    expect(closed).toBe(1);
  });

  test("abandoned observers do not release physical callback resources", async () => {
    const wait = deferred<void>();
    let closed = false;
    const host = createHost();
    await host.install(
      orb({
        name: "slow",
        apiVersion: 1,
        services: {
          slow: {
            create(self) {
              self.resource("connection", {
                open: () => ({}),
                close: () => {
                  closed = true;
                },
              });
              return {};
            },
            methods: {
              async run() {
                await wait.promise;
              },
            },
          },
        },
      }),
    );
    const ref = await host.get("slow", "main");
    const started = deferred<void>();
    const running = host.root(async (ctx) => {
      started.resolve();
      const physical = ref.run(ctx);
      await Promise.resolve();
      ctx.cancel();
      await physical;
    });
    await started.promise;
    await expect(running).rejects.toMatchObject({ code: "cancelled" });
    ref.release();
    expect(closed).toBe(false);
    expect(host.diagnostics().calls).toBe(1);
    wait.resolve();
    await host.close();
    expect(closed).toBe(true);
  });

  test("declared dependencies carry contexts and close after their consumers", async () => {
    const closed: string[] = [];
    const host = createHost();
    await host.install(counter(1, () => closed.push("counter")));
    await host.install(
      orb({
        name: "client",
        apiVersion: 1,
        requires: { counter: { apiVersion: 1, methods: ["increment"] } },
        services: {
          client: {
            create(self) {
              const counter = self.get("counter", "main");
              self.resource("client", {
                open: () => ({}),
                close: async (_, ctx) => {
                  await counter.increment(ctx);
                  closed.push("client");
                },
              });
              return { counter };
            },
            methods: {
              increment({ counter }, ctx) {
                return counter.increment(ctx);
              },
            },
          },
        },
      }),
    );
    const client = await host.get("client", "main");
    await client.increment();
    await host.close();
    expect(closed).toEqual(["client", "counter"]);
  });

  test("hook declarations belong to their orb version", async () => {
    const host = createHost();
    await host.install(
      orb({
        name: "documents",
        apiVersion: 1,
        hooks: { beforeRename: waterfall() },
        services: {
          document: {
            create(self) {
              return { hook: self.hook("beforeRename") };
            },
            methods: {
              rename({ hook }, ctx, title: string) {
                return hook.run(ctx, title);
              },
            },
          },
        },
      }),
    );
    await host.install(
      orb({
        name: "trim",
        apiVersion: 1,
        requires: { documents: { apiVersion: 1, hooks: ["beforeRename"] } },
        contributes: { documents: { beforeRename: (_, title: string) => title.trim() } },
      }),
    );
    const ref = await host.get("document", "main");
    expect(await ref.rename("  title  ")).toBe("title");
    ref.release();
    await host.close();
  });
});

test("engine delivers source events once and reuses unchanged source connections across reload", async () => {
  const host = createHost();
  const callbacks = new Set<(value: number) => void>();
  const source = {
    subscribe(callback: (value: number) => void) {
      callbacks.add(callback);
      return () => {
        callbacks.delete(callback);
      };
    },
  };
  const seen: number[] = [];
  const delivered = deferred<void>();
  const definition = (version: number) =>
    orb({
      name: "events",
      apiVersion: 1,
      create(self) {
        self.on("incoming", source, (_ctx, value) => {
          seen.push(value * version);
          if (seen.length === 2) delivered.resolve();
        });
        return {};
      },
    });
  await host.install(definition(1));
  for (const callback of callbacks) callback(2);
  await new Promise((resolve) => setTimeout(resolve, 0));
  await host.reload([definition(2)]);
  expect(callbacks.size).toBe(1);
  for (const callback of callbacks) callback(3);
  await delivered.promise;
  expect(seen).toEqual([2, 6]);
  await host.close();
  expect(callbacks.size).toBe(0);
});

for (const commit of [false, true])
  test(`${commit ? "committing" : "aborting"} a source replacement handles connections before old calls finish`, async () => {
    const host = createHost();
    const sources = [0, 1].map(() => {
      const callbacks = new Set<(value: number) => void>();
      return {
        callbacks,
        subscribe(callback: (value: number) => void) {
          callbacks.add(callback);
          return () => {
            callbacks.delete(callback);
          };
        },
      };
    });
    const entered = deferred<void>();
    const finish = deferred<void>();
    const delivered = deferred<void>();
    const seen: number[] = [];
    const definition = (version: number) =>
      orb({
        name: "source-replacement",
        apiVersion: 1,
        services: {
          item: {
            create(self) {
              self.on(
                "events",
                sources[version - 1]!,
                (_ctx, value) => {
                  seen.push(value);
                  delivered.resolve();
                },
                { maxPending: 1 },
              );
            },
            methods: {
              async slow() {
                entered.resolve();
                await finish.promise;
                return version;
              },
            },
          },
        },
      });
    await host.install(definition(1));
    const reference = await host.get("item", "main");
    const running = reference.slow();
    await entered.promise;
    try {
      const prepared = await host.prepare([definition(2)]);
      expect(sources.map((source) => source.callbacks.size)).toEqual([1, 1]);
      if (commit) await prepared.commit();
      else await prepared.abort();
      expect(sources.map((source) => source.callbacks.size)).toEqual(commit ? [0, 1] : [1, 0]);
      for (let index = 0; index < 10; index++)
        for (const callback of sources[commit ? 0 : 1]!.callbacks) callback(index);
      for (const callback of sources[commit ? 1 : 0]!.callbacks) callback(42);
      await delivered.promise;
      expect(seen).toEqual([42]);
      expect(host.errors()).toEqual([]);
    } finally {
      finish.resolve();
      expect(await running).toBe(1);
      reference.release();
      await host.close();
    }
    expect(sources.map((source) => source.callbacks.size)).toEqual([0, 0]);
  });
