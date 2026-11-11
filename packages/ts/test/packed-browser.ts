import { readFile } from "node:fs/promises";
import path from "node:path";
import { headless } from "./headless.ts";

const [directory, artifact] = process.argv.slice(2);
if (!directory || !artifact)
  throw new Error(
    "Usage: bun packages/ts/test/packed-browser.ts <installed-project> <arithmetic-source.wasm>",
  );
const modules = path.join(path.resolve(directory), "node_modules/@syntropika");
const packages = new Map([
  ["sdk", path.join(modules, "tembra-sdk")],
  ["engine", path.join(modules, "tembra-engine-wasm")],
]);

/** Resolve a browser export from the installed manifest rather than workspace source conditions. */
async function exported(packageName: string, entry: string): Promise<string> {
  const manifest = JSON.parse(
    await readFile(path.join(packages.get(packageName)!, "package.json"), "utf8"),
  );
  const value = manifest.exports[entry];
  const target =
    typeof value === "string" ? value : (value.browser ?? value.import ?? value.default);
  if (typeof target !== "string" || !target.startsWith("./"))
    throw new Error("Invalid installed export");
  return `/packages/${packageName}/${target.slice(2)}`;
}

const imports = {
  "@syntropika/tembra-engine-wasm": await exported("engine", "."),
  "@syntropika/tembra-engine-wasm/contracts": await exported("engine", "./contracts"),
  "@syntropika/tembra-engine-wasm/compiler": await exported("engine", "./compiler"),
  "@syntropika/tembra-engine-wasm/runtime": await exported("engine", "./runtime"),
};
const entry = await exported("sdk", "./browser");
const rootEntry = await exported("sdk", ".");
// Bundle both public entries as one application, retaining their shared constructor identity.
const bundled = await Bun.build({
  entrypoints: [entry, rootEntry].map((url) =>
    path.join(packages.get("sdk")!, url.slice("/packages/sdk/".length)),
  ),
  naming: { entry: "[name].js", chunk: "chunks/[name]-[hash].js" },
  target: "browser",
  format: "esm",
  splitting: true,
  minify: true,
  external: Object.keys(imports),
});
if (!bundled.success) throw new AggregateError(bundled.logs, "Packed consumer bundle failed");
const output = new Map<string, string>(
  await Promise.all(
    bundled.outputs.map(
      async (file) => [`/bundle/${file.path.slice(2)}`, await file.text()] as const,
    ),
  ),
);
const missing = new Set<string>();
const nonce = crypto.randomUUID().replaceAll("-", "");
const server = Bun.serve({
  hostname: "127.0.0.1",
  port: 0,
  async fetch(request) {
    const url = new URL(request.url);
    if (url.pathname === "/") {
      const bundled = url.searchParams.has("bundle");
      return new Response(
        `<!doctype html><body>running<script nonce="${nonce}" type="importmap">${JSON.stringify({ imports })}</script><script nonce="${nonce}" type="module">
try {
  const {createHost}=await import(${JSON.stringify(bundled ? "/bundle/browser.js" : entry)});
  const {TembraError}=await import(${JSON.stringify(bundled ? "/bundle/index.js" : rootEntry)});
  const host=await createHost();
  try {
    const artifact=new Uint8Array(await (await fetch("/fixture.wasm")).arrayBuffer());
    await host.install({artifact});
    const ref=await host.get("arithmetic::example:integrated/arithmetic@1.0.0","main");
    try {
      if(await ref.double(3)!==6)throw Error("Packed browser call failed");
      let rejected=false;
      try {await host.install({artifact});}catch(error){rejected=error instanceof TembraError && error.reason==="orb_installed";}
      if(!rejected)throw Error("Packed browser duplicate or error identity differs");
      if(await ref.double(4)!==8)throw Error("Packed browser reference invalidated");
    }finally{ref.release();}
  }finally{await host.close();}
  if(host.diagnostics().values!==0)throw Error("Packed browser leaked values");
  document.body.textContent="PACKED_BROWSER_PASS";
}catch(error){document.body.textContent="PACKED_BROWSER_FAIL "+(error.stack??error);}
</script>`,
        {
          headers: {
            "Content-Type": "text/html",
            "Content-Security-Policy": `default-src 'self'; script-src 'self' 'nonce-${nonce}' blob: 'wasm-unsafe-eval'; object-src 'none'; base-uri 'none'`,
          },
        },
      );
    }
    const bundle = output.get(url.pathname);
    if (bundle !== undefined)
      return new Response(bundle, { headers: { "Content-Type": "text/javascript" } });
    if (url.pathname.startsWith("/bundle/")) {
      missing.add(url.pathname);
      return new Response("Missing", { status: 404 });
    }
    if (url.pathname === "/fixture.wasm")
      return new Response(Bun.file(path.resolve(artifact)), {
        headers: { "Content-Type": "application/wasm" },
      });
    const match = /^\/packages\/([^/]+)\/(.+)$/.exec(url.pathname);
    const root = match && packages.get(match[1]!);
    if (!root) return new Response("Missing", { status: 404 });
    const file = path.resolve(root, match![2]!);
    if (!file.startsWith(root + path.sep)) return new Response("Missing", { status: 404 });
    return new Response(Bun.file(file), {
      headers: { "Content-Type": file.endsWith(".wasm") ? "application/wasm" : "text/javascript" },
    });
  },
});
try {
  await headless(server.url.href, "PACKED_BROWSER_PASS", "PACKED_BROWSER_FAIL");
  await headless(new URL("?bundle", server.url).href, "PACKED_BROWSER_PASS", "PACKED_BROWSER_FAIL");
  if (missing.size)
    throw new Error(`Packed consumer requested stale bundle paths: ${[...missing].join(", ")}`);
  console.log("Packed browser entrypoints and consumer bundle passed with no stale chunk requests");
} finally {
  server.stop(true);
}
