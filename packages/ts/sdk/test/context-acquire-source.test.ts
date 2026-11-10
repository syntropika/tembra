// The native twin uses the same prepared source fixtures on Cranelift and Pulley.
import { expect, test } from "bun:test";
import { readFile } from "node:fs/promises";
import path from "node:path";
import { createHost } from "../src/node.ts";
import { verifyContextAcquireScenario } from "./context-acquire-scenario.ts";

const fixtures = process.env.TEMBRA_WASM_FIXTURES;

test.skipIf(!fixtures)("Node: source Context.get selects root-scoped instances", async () => {
  const engine = process.env.TEMBRA_WASM_ENGINE;
  const host = createHost(
    engine ? { isolated: { engine: new Uint8Array(await readFile(engine)) } } : {},
  );
  try {
    await verifyContextAcquireScenario(
      host,
      async (name) => new Uint8Array(await readFile(path.join(fixtures!, `${name}.wasm`))),
    );
  } finally {
    await host.close();
  }
  expect(host.diagnostics().references).toBe(0);
  expect(host.diagnostics().pendingJobs).toBe(0);
});
