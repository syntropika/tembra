// Install two built Orb artifacts through the ordinary Node host.
// Run with Bun: bun consumer.ts <calculator.wasm> <arithmetic.wasm>
import { createServer } from "node:http";
import { once } from "node:events";
import { createHost } from "@syntropika/tembra-sdk/node";

const [calculatorFile, arithmeticFile] = process.argv.slice(2);
if (!calculatorFile || !arithmeticFile)
  throw new Error("Usage: bun consumer.ts <calculator.wasm> <arithmetic.wasm>");

// A local API makes the complete HTTP example reproducible without an external service.
let requests = 0;
const api = createServer((request, response) => {
  if (request.url !== "/values") {
    response.writeHead(404).end();
    return;
  }
  requests++;
  response.setHeader("content-type", "application/json");
  response.end(JSON.stringify({ values: [10, 20, 30] }));
});
api.listen(0, "127.0.0.1");
await once(api, "listening");
const address = api.address();
if (!address || typeof address === "string") throw new Error("Missing local API address");
const origin = `http://127.0.0.1:${address.port}`;
const host = createHost({
  isolated: {
    providers: { network: ["outbound"] },
    onLog: (record) => console.error(`${record.orb} [${record.level}] ${record.message}`),
  },
});
try {
  await host.load(arithmeticFile);
  // The artifact names the WIT import; the installer selects which installed Orb provides it.
  await host.load(calculatorFile, {
    requires: { "example:integrated/arithmetic@1.0.0": { orb: "arithmetic", apiVersion: 1 } },
    grants: { http: [{ provider: "outbound", origins: [origin] }] },
  });
  const calculation = await host.get("calculator::example:integrated/calculation@1.0.0", "main");
  console.log(await calculation.sum({ values: [3, 4, 5] }));
  console.log({ state: calculation.state.value, resource: await calculation["resource-value"]() });
  const fetched = await calculation["fetch-and-sum"](`${origin}/values`);
  if (fetched !== 120 || requests !== 1) throw new Error("The HTTP calculation did not run");
  console.log({ fetched });
  calculation.release();
} finally {
  try {
    await host.close();
  } finally {
    const closed = once(api, "close");
    api.close();
    await closed;
  }
}
