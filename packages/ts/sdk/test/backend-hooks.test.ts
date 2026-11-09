import { test, expect, spyOn } from "bun:test";
import { createHost } from "../src/node.ts";
import { disposeOrb, isolate, type AdaptedOrb } from "#tembra-adapters";
import { KernelOp } from "../src/protocol.ts";
import type { OrbDefinition } from "../src/authoring/types.ts";

test("failed installation preserves its cause when backend disposal also fails", async () => {
  const cleanup = new Error("Backend disposal failed");
  const host = createHost({
    principal: "test",
    ...{
      [isolate]: async () =>
        ({
          name: "invalid-dependency",
          apiVersion: 1,
          requires: { missing: { apiVersion: 1, methods: ["read"] } },
          [disposeOrb]: () => {
            throw cleanup;
          },
        }) as AdaptedOrb,
    },
  });
  try {
    const failure = await host.install({ artifact: new Uint8Array() }).catch((error) => error);
    expect(failure).toBeInstanceOf(AggregateError);
    expect(failure.errors[0]).toMatchObject({ reason: "dependency_service" });
    expect(failure.errors[1]).toBeInstanceOf(AggregateError);
    expect(failure.errors[1].errors).toEqual([cleanup]);
    expect(host.diagnostics().orbs).toBe(0);
  } finally {
    await host.close();
  }
});

// Knowing an internal hook's description must not bypass normal JavaScript factory rules.
test("ordinary definitions cannot forge interpreter factory hooks", async () => {
  const host = createHost();
  try {
    await expect(
      host.install({
        name: "forged-factory",
        apiVersion: 1,
        create: async () => ({}),
        [Symbol.for("tembra.declare-factory")]: async () => ({}),
      }),
    ).rejects.toMatchObject({ reason: "async_factory" });
  } finally {
    await host.close();
  }
});

test("close waits for materialization and disposes a revision that cannot be installed", async () => {
  const started = Promise.withResolvers<void>();
  const prepared = Promise.withResolvers<OrbDefinition>();
  let disposed = false;
  let closed = false;
  const host = createHost({
    principal: "test",
    ...{
      [isolate]: () => {
        started.resolve();
        return prepared.promise;
      },
    },
  });
  const installing = host.install({ artifact: new Uint8Array() });
  const rejected = installing.catch((error: unknown) => error);
  await started.promise;
  const closing = host.close().then(() => {
    closed = true;
  });
  try {
    await Bun.sleep(0);
    expect(closed).toBe(false);
  } finally {
    prepared.resolve({
      name: "pending-backend",
      apiVersion: 1,
      [disposeOrb]: () => {
        disposed = true;
      },
    } as AdaptedOrb);
    await closing;
  }
  expect(await rejected).toMatchObject({ reason: "host" });
  expect(disposed).toBe(true);
  expect(host.diagnostics().pendingJobs).toBe(0);
});

test("artifact installation cannot replace an installed identity or invalidate its references", async () => {
  let disposed = 0;
  const host = createHost({
    principal: "test",
    ...{
      [isolate]: async () =>
        ({
          name: "installed",
          apiVersion: 1,
          services: { value: { methods: { read: () => 99 } } },
          [disposeOrb]: () => {
            disposed++;
          },
        }) as AdaptedOrb,
    },
  });
  await host.install({
    name: "installed",
    apiVersion: 1,
    services: { value: { methods: { read: () => 42 } } },
  });
  const reference = await host.get("value", "main");
  const version = host.diagnostics().codeVersion;
  try {
    await expect(host.install({ artifact: new Uint8Array() })).rejects.toMatchObject({
      reason: "orb_installed",
    });
    expect(await reference.read()).toBe(42);
    expect(host.diagnostics().codeVersion).toBe(version);
    expect(disposed).toBe(1);
  } finally {
    reference.release();
    await host.close();
  }
});

test("concurrent artifact preparations recheck the committed identity before installation", async () => {
  const first = Promise.withResolvers<OrbDefinition>();
  const second = Promise.withResolvers<OrbDefinition>();
  let count = 0,
    disposed = 0;
  const host = createHost({
    principal: "test",
    ...{ [isolate]: () => (++count === 1 ? first.promise : second.promise) },
  });
  const a = host.install({ artifact: new Uint8Array() });
  const b = host.install({ artifact: new Uint8Array() });
  const rejected = b.catch((error: unknown) => error);
  const candidate = () =>
    ({
      name: "same",
      apiVersion: 1,
      [disposeOrb]: () => {
        disposed++;
      },
    }) as AdaptedOrb;
  try {
    first.resolve(candidate());
    await a;
    const version = host.diagnostics().codeVersion;
    second.resolve(candidate());
    expect(await rejected).toMatchObject({ reason: "orb_installed" });
    expect(host.diagnostics().codeVersion).toBe(version);
    expect(disposed).toBe(1);
  } finally {
    await host.close();
  }
  expect(disposed).toBe(2);
});

for (const phase of ["declaration", "factory"] as const)
  test(`reload retains its ${phase} failure when candidate disposal also fails`, async () => {
    const cleanup = new Error("candidate disposal failed");
    const original = new Error("candidate factory failed");
    const host = createHost({
      principal: "test",
      ...{
        [isolate]: async () =>
          ({
            name: "candidate",
            apiVersion: 1,
            ...(phase === "declaration"
              ? { requires: { missing: { apiVersion: 1, methods: ["read"] } } }
              : {
                  create() {
                    throw original;
                  },
                }),
            [disposeOrb]() {
              throw cleanup;
            },
          }) as AdaptedOrb,
      },
    });
    try {
      const failure = await host.reload([{ artifact: new Uint8Array() }]).catch((error) => error);
      expect(failure).toBeInstanceOf(AggregateError);
      if (phase === "declaration")
        expect(failure.errors[0]).toMatchObject({ reason: "dependency_service" });
      else expect(failure.errors[0].message).toContain("candidate factory failed");
      expect(failure.errors[1].errors).toEqual([cleanup]);
    } finally {
      await host.close();
    }
  });

for (const phase of ["commit", "abort"] as const)
  test(`reload retains its ${phase} failure when candidate disposal also fails`, async () => {
    const original = new Error(`candidate ${phase} failed`);
    const cleanup = new Error("candidate disposal failed");
    const host = createHost({
      principal: "test",
      ...{
        [isolate]: async () =>
          ({
            name: "candidate",
            apiVersion: 1,
            [disposeOrb]() {
              throw cleanup;
            },
          }) as AdaptedOrb,
      },
    });
    await host.install({ name: "candidate", apiVersion: 1 });
    const prepared = await host.prepare([{ artifact: new Uint8Array() }]);
    const command = host.session.kernel.port.command.bind(host.session.kernel.port);
    const run = host.session.programs.run.bind(host.session.programs);
    const injected =
      phase === "commit"
        ? spyOn(host.session.kernel.port, "command").mockImplementation((op, input) => {
            if (op === KernelOp.ProgramCommit) throw original;
            return command(op, input);
          })
        : spyOn(host.session.programs, "run").mockImplementation(async (program) => {
            try {
              await run(program);
            } catch {
              /* The real abort has physically settled. */
            }
            throw original;
          });
    try {
      const failure = (await prepared[phase]().catch((error: unknown) => error)) as AggregateError;
      expect(failure).toBeInstanceOf(AggregateError);
      expect(failure.errors[0]).toBe(original);
      expect(failure.errors[1].errors).toEqual([cleanup]);
      expect(prepared.status).toBe("aborted");
    } finally {
      injected.mockRestore();
      await host.close();
    }
  });
