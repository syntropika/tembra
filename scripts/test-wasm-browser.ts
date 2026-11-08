import path from "node:path";
import { headless } from "../packages/ts/test/headless.ts";

const fixtures = process.env.TEMBRA_WASM_FIXTURES;
if (!fixtures) throw new Error("Set TEMBRA_WASM_FIXTURES to the built qualification artifacts");
const root = path.resolve(import.meta.dir, "..");
const engine =
  process.env.TEMBRA_WASM_BROWSER_ENGINE ??
  path.join(root, "packages/ts/engine-wasm/dist/interpreter");
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
    if (/^\/[\w-]+\.wasm$/.test(pathname))
      return new Response(Bun.file(path.join(fixtures, pathname.slice(1))));
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
