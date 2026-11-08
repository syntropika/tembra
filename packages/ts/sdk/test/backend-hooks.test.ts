import { test, expect } from "bun:test";
import { createHost } from "../src/node.ts";
import { disposeOrb, materialize, type AdaptedOrb } from "@syntropika/tembra-sdk/internal/adapters";
import type { OrbDefinition } from "../src/authoring/types.ts";

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
  const host = createHost();
  const started = Promise.withResolvers<void>();
  const prepared = Promise.withResolvers<OrbDefinition>();
  let disposed = false;
  let closed = false;
  const candidate: AdaptedOrb = {
    name: "pending-backend",
    apiVersion: 1,
    [materialize]: () => {
      started.resolve();
      return prepared.promise;
    },
  };
  const installing = host.install(candidate);
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
      name: candidate.name,
      apiVersion: candidate.apiVersion,
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
