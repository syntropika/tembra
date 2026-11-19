// Run a prepared compiled probe through the browser host's JS component adapter under Node, whose
// JSPI is behind `--experimental-wasm-jspi`, and print the error of each call as one JSON line:
// `trap()` and then `spin(1)`. Usage: node --experimental-wasm-jspi <this> <adapter> <artifact>.
import { readFile } from "node:fs/promises";
import { pathToFileURL } from "node:url";

const [adapter, artifact] = process.argv.slice(2);
// The adapter's cores and the contract engine are fetched by module URL, as a browser fetches them.
const fetched = globalThis.fetch;
globalThis.fetch = async (input, init) => {
  const url = new URL(input instanceof Request ? input.url : input);
  if (url.protocol !== "file:") return fetched(input, init);
  return new Response(await readFile(url), { headers: { "content-type": "application/wasm" } });
};
const { createHost, memoryFilesystem } = await import("@syntropika/tembra-sdk/browser");
const component = (await import(pathToFileURL(adapter).href)).default;
const host = await createHost({
  isolated: { components: [component], providers: { filesystem: { memory: memoryFilesystem() } } },
});
try {
  await host.install({
    artifact: new Uint8Array(await readFile(artifact)),
    grants: { filesystem: [{ provider: "memory", mount: "/data", writable: false }] },
  });
  const reference = await host.get("probe::example:probe/probe@1.0.0", "main");
  for (const call of [() => reference.trap(), () => reference.spin(1)]) {
    try {
      await call();
      console.log(JSON.stringify({ ok: true }));
    } catch ({ code, reason, message }) {
      console.log(JSON.stringify({ code, reason, message }));
    }
  }
  reference.release();
} finally {
  await host.close();
}
