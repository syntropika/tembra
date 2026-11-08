import { readFile } from "node:fs/promises";
import { createHost, createInterpreter } from "@syntropika/tembra-sdk/node";

const [engineFile, sourceFile, componentFile] = process.argv.slice(2);
if (!engineFile || !sourceFile || !componentFile)
  throw new Error("Usage: node consumer.mjs <engine.wasm> <source.json> <arithmetic.json>");

const interpreter = createInterpreter({ engine: new Uint8Array(await readFile(engineFile)) });
const arithmetic = JSON.parse(await readFile(componentFile, "utf8"));
const host = createHost();
try {
  await host.install([
    interpreter.orb({ name: "arithmetic", apiVersion: 1, source: arithmetic }),
    interpreter.orb({
      name: "calculator",
      apiVersion: 1,
      source: JSON.parse(await readFile(sourceFile, "utf8")),
      bindings: {
        "example:integrated/arithmetic@1.0.0": {
          kind: "orb",
          name: "arithmetic",
          api_version: 1,
          world: "arithmetic-provider",
        },
      },
    }),
  ]);
  const calculation = await host.get("calculator::example:integrated/calculation@1.0.0", "main");
  console.log(await calculation.sum({ values: [3, 4, 5] }));
  console.log({ state: calculation.state.value, resource: await calculation["resource-value"]() });
  calculation.release();
} finally {
  await host.close();
}
if (interpreter.instances() !== 0)
  throw new Error("An interpreter Store outlived its physical owners");
