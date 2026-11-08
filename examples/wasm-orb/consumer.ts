// Install two built Orb artifacts through the ordinary Node host.
// Run with Bun: bun consumer.ts <calculator.wasm> <arithmetic.wasm>
import { createHost } from "@syntropika/tembra-sdk/node";

const [calculatorFile, arithmeticFile] = process.argv.slice(2);
if (!calculatorFile || !arithmeticFile)
  throw new Error("Usage: bun consumer.ts <calculator.wasm> <arithmetic.wasm>");

const host = createHost({
  isolated: {
    onLog: (record) => console.error(`${record.orb} [${record.level}] ${record.message}`),
  },
});
try {
  await host.load(arithmeticFile);
  // The artifact names the WIT import; the installer selects which installed Orb provides it.
  await host.load(calculatorFile, {
    requires: { "example:integrated/arithmetic@1.0.0": { orb: "arithmetic", apiVersion: 1 } },
  });
  const calculation = await host.get("calculator::example:integrated/calculation@1.0.0", "main");
  console.log(await calculation.sum({ values: [3, 4, 5] }));
  console.log({ state: calculation.state.value, resource: await calculation["resource-value"]() });
  calculation.release();
} finally {
  await host.close();
}
