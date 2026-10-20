/* oxlint-disable no-await-in-loop -- Each source revision, core commit and GC observation depends on the preceding round. */
import assert from "node:assert/strict";
import { mkdir, rm, writeFile } from "node:fs/promises";
import path from "node:path";
import { test } from "node:test";
import { setImmediate as tick } from "node:timers/promises";

import { createHost } from "@tembra/sdk/node";
import { assertQuiescent, structuredError } from "@tembra/testing";

import { Counter, sourceFixture, updateSource, installBias } from "./source-fixture.ts";

async function collect() {
  for (let turn = 0; turn < 8; turn += 1) {
    await tick();
    globalThis.gc?.();
  }
}
await test(
  "source evaluation budget bounds hundreds of core reloads and releases managed graphs",
  { timeout: 120_000 },
  async () => {
    const root = await sourceFixture();
    const host = await createHost({ moduleBase: root });
    const weak: WeakRef<() => number>[] = [];
    globalThis.__tembraSourceTest = {
      capability: {},
      closed: 0,
      wait: async () => undefined,
      observe(read: () => number) {
        weak.push(new WeakRef(read));
      },
    };
    let reference;
    let total = 0;
    try {
      await installBias(host);
      await host.load("./facet.ts");
      reference = await host.get(Counter, {});
      total += 1;
      assert.equal(await reference.increment(), total + 13);
      await collect();
      const baseline = process.memoryUsage().heapUsed;
      const limit = host.diagnostics().loader.source.maxEvaluations;
      for (let generation = 1; generation < limit; generation += 1) {
        const step = (generation % 2) + 1;
        await updateSource(root, step);
        await host.reload(["./facet.ts"]);
        total += step;
        assert.equal(await reference.increment(), total + 13);
        assert.equal(await reference.hook(10), 10 + step);
        if (generation % 32 === 0) {
          await collect();
        }
      }
      const before = host.diagnostics();
      for (let attempt = 0; attempt < 100; attempt += 1) {
        await assert.rejects(host.reload(["./facet.ts"]), (error) => {
          structuredError(error);
          return (
            error.reason === "loader_budget_exhausted" && error.details.profile === "node-source-v1"
          );
        });
      }
      assert.equal(host.diagnostics().codeVersion, before.codeVersion);
      assert.equal(host.diagnostics().loader.source.evaluations, limit);
      assert.equal(reference.count.value, total);
      assert.equal(host.diagnostics().bridge.callbacks, before.bridge.callbacks);
      reference.release();
      reference = undefined;
      await host.close();
      await collect();
      const final = process.memoryUsage().heapUsed;
      assertQuiescent({
        callbacks: host.diagnostics().bridge.callbacks,
        values: host.diagnostics().bridge.values,
        jobs: host.diagnostics().bridge.jobs,
      });
      assert.equal(host.diagnostics().generations.length, 0);
      assert.equal(globalThis.__tembraSourceTest.closed, 1);
      const replacementHost = await createHost({ moduleBase: root });
      try {
        await assert.rejects(replacementHost.load("./facet.ts"), {
          reason: "loader_budget_exhausted",
        });
        assert.equal(replacementHost.diagnostics().loader.source.evaluations, limit);
        assert.equal(replacementHost.diagnostics().loader.source.requiresProcessRotation, true);
      } finally {
        await replacementHost.close();
      }
      const alive = weak.filter((ref) => ref.deref() !== undefined).length;
      if (globalThis.gc) {
        assert.equal(alive, 0);
        assert.ok(
          final - baseline < 96 * 1024 * 1024,
          "released source graphs must not retain the large per-generation payloads",
        );
      }
      const evidence = {
        node: process.version,
        generations: limit,
        reloads: limit - 1,
        refused: 100,
        baselineHeap: baseline,
        finalHeap: final,
        observedCallbacks: weak.length,
        alive,
        managedCallbacks: host.diagnostics().bridge.callbacks,
      };
      console.log(JSON.stringify(evidence));
      if (process.env.TEMBRA_MEMORY_REPORT_DIR !== undefined) {
        await mkdir(process.env.TEMBRA_MEMORY_REPORT_DIR, { recursive: true });
        await writeFile(
          path.join(process.env.TEMBRA_MEMORY_REPORT_DIR, `source-${process.versions.node}.json`),
          JSON.stringify(evidence, null, 2),
        );
      }
    } finally {
      reference?.release();
      await host.close();
      Reflect.deleteProperty(globalThis, "__tembraSourceTest");
      await rm(root, { recursive: true, force: true });
    }
  },
);
