import { createHash } from "node:crypto";
import path from "node:path";
import { compileContract } from "@syntropika/tembra-engine-node";
import { headless } from "../packages/ts/test/headless.ts";

const engine = process.env.TEMBRA_WASM_BROWSER_ENGINE;
const orb = process.env.TEMBRA_WASM_ORB;
if (!engine || !orb)
  throw new Error("Set TEMBRA_WASM_BROWSER_ENGINE and TEMBRA_WASM_ORB to prepared artifacts");
const root = path.resolve(import.meta.dir, "..");
const filesSource =
  "globalThis.__tembraOrb={services:{files:{methods:{read:async(_e,_c,path)=>tembra.filesystem.readText(path),write:async(_e,_c,path,value)=>tembra.filesystem.writeText(path,value)}}}};";
const files = {
  format: "tembra-interpreter-source-v1",
  source: filesSource,
  sha256: createHash("sha256").update(filesSource).digest("hex"),
  typesSha256: "",
  contract: Array.from(
    compileContract(path.join(root, "crates/executor-wasm/tests/wit/storage"), "storage"),
  ),
  world: "storage",
};
const guardSource = `globalThis.__tembraOrb={services:{arithmetic:{
  create(self){if(self.key[0]==="broken")throw new Error("factory refused");return {};},
  methods:{double:async(_e,_c,value)=>{
    if(value===1){while(true){}}
    if(value===2)return new Promise(()=>{});
    if(value===4){tembra.wait(50);return 42;}
    if(value===5)throw new Error("upload interrupted");
    return value*2;
  }}
}}};`;
const guard = {
  format: "tembra-interpreter-source-v1",
  source: guardSource,
  sha256: createHash("sha256").update(guardSource).digest("hex"),
  typesSha256: "",
  contract: Array.from(
    compileContract(path.join(root, "examples/wasm-orb"), "arithmetic-provider"),
  ),
  world: "arithmetic-provider",
};
const sdk = await Bun.build({
  entrypoints: [path.join(root, "packages/ts/sdk/src/browser.ts")],
  target: "browser",
  format: "esm",
  plugins: [
    {
      name: "engine-assets",
      setup(builder) {
        builder.onResolve(
          { filter: /^@syntropika\/tembra-engine-wasm(?:\/contracts)?$/ },
          ({ path: imported }) => ({
            path: `/packages/ts/engine-wasm/dist/${imported.endsWith("/contracts") ? "contracts" : "tembra"}.js`,
            external: true,
          }),
        );
      },
    },
  ],
});
const fixture = await Bun.build({
  entrypoints: [path.join(root, "packages/ts/sdk/test/interpreter-browser.ts")],
  target: "browser",
  format: "esm",
  plugins: [
    {
      name: "sdk-entry",
      setup(builder) {
        builder.onResolve({ filter: /\/src\/browser\.ts$/ }, () => ({
          path: "/sdk.js",
          external: true,
        }));
      },
    },
  ],
});
if (!sdk.success || !fixture.success)
  throw new AggregateError([...sdk.logs, ...fixture.logs], "Browser fixture build failed");
const sources = new Map([
  ["/sdk.js", await sdk.outputs[0]!.text()],
  ["/fixture.js", await fixture.outputs[0]!.text()],
]);
const server = Bun.serve({
  hostname: "127.0.0.1",
  port: 0,
  async fetch(request) {
    const pathname = new URL(request.url).pathname;
    if (pathname === "/")
      return new Response(
        '<!doctype html><body>running<script type="module">import {verifyBrowserInterpreter} from "/fixture.js";try {await verifyBrowserInterpreter();document.body.textContent="TEMBRA_WASM_PASS";} catch(error) {document.body.textContent="TEMBRA_WASM_FAIL "+(error.stack??error);}</script>',
        { headers: { "Content-Type": "text/html" } },
      );
    if (sources.has(pathname))
      return new Response(sources.get(pathname), {
        headers: { "Content-Type": "application/javascript" },
      });
    if (pathname === "/orb.json") return new Response(Bun.file(orb));
    if (pathname === "/guard.json") return Response.json(guard);
    if (pathname === "/files.json") return Response.json(files);
    const base = pathname.startsWith("/engine/")
      ? path.resolve(engine)
      : path.join(root, "packages/ts/engine-wasm/dist");
    const relative = pathname.startsWith("/engine/")
      ? pathname.slice(8)
      : pathname.replace(/^\/packages\/ts\/engine-wasm\/dist\//, "");
    const resolved = path.resolve(base, relative);
    if (!resolved.startsWith(`${base}${path.sep}`))
      return new Response("Not found", { status: 404 });
    const file = Bun.file(resolved);
    return (await file.exists()) ? new Response(file) : new Response("Not found", { status: 404 });
  },
});
try {
  await headless(server.url.href, "TEMBRA_WASM_PASS", "TEMBRA_WASM_FAIL");
  console.log("Actual browser interpreter facade, memory filesystem and OPFS acceptance passed");
} finally {
  server.stop(true);
}
