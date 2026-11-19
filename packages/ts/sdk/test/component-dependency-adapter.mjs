// Run the prepared compiled pipeline through the browser host's JS component adapter under Node,
// whose JSPI is behind `--experimental-wasm-jspi`, beside TypeScript dependencies that refuse, and
// print the error of two `quadruple` calls as JSON lines.
// Usage: node --experimental-wasm-jspi <this> <adapter> <artifact> <signatures JSON>.
import { readFile } from "node:fs/promises";
import { pathToFileURL } from "node:url";
import { signature } from "#tembra-adapters";
import { dependencies, requires } from "./component-dependencies.mjs";

const [adapter, artifact, signatures] = process.argv.slice(2);
// The adapter's cores and the contract engine are fetched by module URL, as a browser fetches them.
const fetched = globalThis.fetch;
globalThis.fetch = async (input, init) => {
  const url = new URL(input instanceof Request ? input.url : input);
  if (url.protocol !== "file:") return fetched(input, init);
  return new Response(await readFile(url), { headers: { "content-type": "application/wasm" } });
};
const { createHost } = await import("@syntropika/tembra-sdk/browser");
const component = (await import(pathToFileURL(adapter).href)).default;
const host = await createHost({ isolated: { components: [component] } });
try {
  await host.install([
    ...dependencies(JSON.parse(signatures), signature),
    { artifact: new Uint8Array(await readFile(artifact)), requires },
  ]);
  const reference = await host.get("pipeline::example:pipeline/pipeline@1.0.0", "main");
  for (let call = 0; call < 2; call += 1) {
    try {
      await reference.quadruple(1);
      console.log(JSON.stringify({ ok: true }));
    } catch ({ code, reason, message }) {
      console.log(JSON.stringify({ code, reason, message }));
    }
  }
  reference.release();
} finally {
  await host.close();
}
