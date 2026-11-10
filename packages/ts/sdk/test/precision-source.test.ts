// Node entry point of the exact u64/s64 scenario on real source Orbs. Runs with
// TEMBRA_WASM_FIXTURES and the packaged interpreter; see `precision-scenario.ts`.
import { expect, test } from "bun:test";
import { readFile } from "node:fs/promises";
import path from "node:path";
import { createHost } from "../src/node.ts";
import { verifyPrecisionScenario } from "./precision-scenario.ts";

const fixtures = process.env.TEMBRA_WASM_FIXTURES;

test.skipIf(!fixtures)("Node: exact u64/s64 between the application and source Orbs", async () => {
  const engine = process.env.TEMBRA_WASM_ENGINE;
  const host = createHost(
    engine ? { isolated: { engine: new Uint8Array(await readFile(engine)) } } : {},
  );
  try {
    const report = await verifyPrecisionScenario(
      host,
      async (name) => new Uint8Array(await readFile(path.join(fixtures!, `${name}.wasm`))),
    );
    expect(report).toContain("application -> source -> source");
  } finally {
    await host.close();
  }
  expect(host.diagnostics().values).toBe(0);
  expect(host.diagnostics().pendingJobs).toBe(0);
});
