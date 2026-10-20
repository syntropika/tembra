import path from "node:path";

const testing = path.resolve(import.meta.dirname, "..");
const sdk = path.resolve(testing, "../../packages/sdk");
const result = await Bun.build({
  entrypoints: [path.join(testing, "test/browser/browser-host.ts")],
  target: "browser",
  format: "esm",
});
if (!result.success || result.outputs[0] === undefined) {
  throw new Error(result.logs.map(String).join("\n"));
}
const code = await result.outputs[0].text();
const modules = await Bun.build({
  entrypoints: [path.join(testing, "test/browser/browser-module-facet.ts")],
  target: "browser",
  format: "esm",
  plugins: [
    {
      name: "native-module-dependency",
      setup(builder) {
        builder.onResolve({ filter: /browser-module-dependency/u }, () => ({
          path: "./browser-module-dependency.js",
          external: true,
        }));
      },
    },
  ],
});
const workers = await Bun.build({
  entrypoints: [path.join(testing, "test/browser/browser-module-worker.ts")],
  target: "browser",
  format: "esm",
});
if (!modules.success || !workers.success || !modules.outputs[0] || !workers.outputs[0]) {
  throw new Error([...modules.logs, ...workers.logs].map(String).join("\n"));
}
const moduleCode = await modules.outputs[0].text();
const workerCode = await workers.outputs[0].text();
const managedWorkers = await Bun.build({
  entrypoints: [path.join(sdk, "src/browser/worker.ts")],
  target: "browser",
  format: "esm",
});
const managedFacets = await Bun.build({
  entrypoints: [path.join(testing, "test/browser/browser-manager-facet.ts")],
  target: "browser",
  format: "esm",
});
if (
  !managedWorkers.success ||
  !managedFacets.success ||
  !managedWorkers.outputs[0] ||
  !managedFacets.outputs[0]
) {
  throw new Error([...managedWorkers.logs, ...managedFacets.logs].map(String).join("\n"));
}
const managedWorkerCode = await managedWorkers.outputs[0].text();
const managedFacetCode = await managedFacets.outputs[0].text();
const server = Bun.serve({
  port: Number(process.env.PORT ?? 0),
  fetch(request) {
    const url = new URL(request.url);
    if (url.pathname === "/manager-worker.js") {
      return new Response(managedWorkerCode, { headers: { "content-type": "text/javascript" } });
    }
    if (url.pathname === "/manager-liveness-facet.js") {
      return new Response("export default {name:'managed-liveness',uses:[],setup(){}};", {
        headers: { "content-type": "text/javascript" },
      });
    }
    if (url.pathname === "/manager-facet.js") {
      return new Response(managedFacetCode, { headers: { "content-type": "text/javascript" } });
    }
    if (/^\/modules\/v[12]\/facet\.js$/u.test(url.pathname)) {
      return new Response(moduleCode, { headers: { "content-type": "text/javascript" } });
    }
    if (/^\/modules\/v[12]\/browser-module-dependency\.js$/u.test(url.pathname)) {
      return new Response(`export const step=${url.pathname.includes("/v2/") ? 2 : 1};`, {
        headers: { "content-type": "text/javascript" },
      });
    }
    if (url.pathname === "/module-worker.js") {
      return new Response(workerCode, { headers: { "content-type": "text/javascript" } });
    }
    if (url.pathname === "/missing-module.js") {
      return new Response("missing", { status: 404 });
    }
    if (url.pathname === "/invalid-module.js") {
      return new Response("export const invalid: number=1;", {
        headers: { "content-type": "text/javascript" },
      });
    }
    if (url.pathname === "/pending-module.js") {
      return new Response(
        "globalThis.__browserModuleEntered=true;await globalThis.__browserModuleGate;export default {name:'pending-browser-module',uses:[],setup(){}};",
        { headers: { "content-type": "text/javascript" } },
      );
    }
    if (url.pathname === "/test.js") {
      return new Response(code, { headers: { "content-type": "text/javascript" } });
    }
    if (url.pathname.startsWith("/wasm/")) {
      return new Response(Bun.file(path.join(sdk, "dist", url.pathname)), {
        headers: {
          "content-type": url.pathname.endsWith(".wasm") ? "application/wasm" : "text/javascript",
        },
      });
    }
    return new Response(
      `<!doctype html><html lang="en"><title>Host acceptance</title><output>running</output><script type="importmap">{"imports":{"acceptance-facet":"/modules/v1/facet.js","acceptance/facet.js":"/modules/v1/facet.js"}}</script><script type="module">import {browserAcceptance} from '/test.js';try {const results=await browserAcceptance();document.querySelector('output').textContent=JSON.stringify({passed:results});}catch(error){document.querySelector('output').textContent=JSON.stringify({error:String(error),stack:error.stack});}</script>`,
      { headers: { "content-type": "text/html" } },
    );
  },
});
console.log(server.url.href);
