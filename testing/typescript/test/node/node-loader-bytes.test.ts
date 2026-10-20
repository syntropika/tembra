import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { test } from "node:test";

import type { EsmArtifactV1 } from "@tembra/sdk";
import { createHost } from "@tembra/sdk/node";
import { structuredError } from "@tembra/testing";

function artifact(bytes: number): EsmArtifactV1 {
  const source = "export default {};//".padEnd(bytes, "x");
  return {
    format: "tembra-esm-v1",
    identity: "byte-budget",
    source,
    exports: ["default"],
    sha256: createHash("sha256").update(source).digest("hex"),
  };
}

await test("per-artifact and cumulative UTF-8 limits are enforced independently of the count limit", async () => {
  const host = await createHost();
  try {
    await assert.rejects(host.load(artifact(1_048_577)), { reason: "artifact_size" });
    assert.equal(host.diagnostics().loader.evaluations, 0);
    const compiled = artifact(1_048_576);
    for (let i = 0; i < 8; i += 1) {
      // oxlint-disable-next-line no-await-in-loop -- Each rejected export consumes exactly one admitted source allocation.
      await assert.rejects(host.load(compiled), { reason: "facet" });
    }
    const before = host.diagnostics();
    assert.equal(before.loader.sourceBytes, 8_388_608);
    assert.equal(before.loader.evaluations, 8);
    await assert.rejects(host.load(artifact(100)), (error) => {
      structuredError(error);
      assert.equal(error.reason, "loader_budget_exhausted");
      assert.equal(error.details.dimension, "sourceBytes");
      return true;
    });
    assert.equal(host.diagnostics().loader.evaluations, 8);
    assert.equal(host.diagnostics().loader.inFlight, null);
    assert.equal(host.diagnostics().codeVersion, before.codeVersion);
    await host.install({
      name: "still-live",
      uses: [],
      setup() {
        /* No declarations are needed to prove continued admission. */
      },
    });
  } finally {
    await host.close();
  }
});
