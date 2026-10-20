import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { test } from "node:test";

import type { EsmArtifactV1 } from "@tembra/sdk";
import { createHost } from "@tembra/sdk/node";
import { deferredVoid, defined } from "@tembra/testing";

function artifact(source: string): EsmArtifactV1 {
  return {
    format: "tembra-esm-v1",
    identity: "reservation-diagnostics",
    source,
    sha256: createHash("sha256").update(source).digest("hex"),
    exports: ["default"],
  };
}

await test("in-flight reservations include queued source and compiled entries without counting submitted work twice", async () => {
  const directory = await mkdtemp(path.join(tmpdir(), "tembra-reservations-"));
  const host = await createHost({ moduleBase: directory });
  const { promise: entered, resolve: enter } = deferredVoid();
  const { promise: gate, resolve: finish } = deferredVoid();
  globalThis.__tembraLoaderGate = { enter, gate };
  const source = 'const label: string = "queued-λ"; export default {name:label,uses:[],setup(){}};';
  const queued = artifact("export default {name:'queued-artifact',uses:[],setup(){}};");
  try {
    await writeFile(path.join(directory, "facet.ts"), source);
    const before = host.diagnostics().loader;
    const pending = host.reload([
      artifact(
        "const p=globalThis.__tembraLoaderGate;p.enter();await p.gate;export default {name:'held-artifact',uses:[],setup(){}};",
      ),
      "./facet.ts",
      queued,
    ]);
    try {
      await entered;
      const diagnostics = host.diagnostics().loader;
      const active = defined(diagnostics.inFlight ?? undefined);
      assert.equal(active.stage, "evaluating");
      assert.equal(active.reservedEvaluations, 2);
      assert.equal(
        active.reservedSourceBytes,
        new TextEncoder().encode(source).byteLength +
          new TextEncoder().encode(queued.source).byteLength,
      );
      assert.equal(diagnostics.evaluations, before.evaluations + 1);
      assert.equal(diagnostics.source.evaluations, before.source.evaluations);
    } finally {
      finish();
      await pending;
    }
    assert.equal(host.diagnostics().loader.inFlight, null);
    assert.equal(host.diagnostics().loader.source.evaluations, before.source.evaluations + 1);
    assert.equal(host.diagnostics().loader.evaluations, before.evaluations + 2);
  } finally {
    finish();
    Reflect.deleteProperty(globalThis, "__tembraLoaderGate");
    await host.close();
    await rm(directory, { recursive: true, force: true });
  }
});
