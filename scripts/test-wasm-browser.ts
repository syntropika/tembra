import { realpathSync } from "node:fs";
import path from "node:path";
import { headless } from "../packages/ts/test/headless.ts";

const fixtures = process.env.TEMBRA_WASM_FIXTURES;
if (!fixtures) throw new Error("Set TEMBRA_WASM_FIXTURES to the built qualification artifacts");
const root = path.resolve(import.meta.dir, "..");
const sdkPackage = path.join(root, "packages/ts/sdk");
const enginePackage = realpathSync(path.join(root, "packages/ts/engine-wasm"));
const enginePrefix = "/packages/ts/engine-wasm/";
// An alternate engine replaces only the explicit override; the default run keeps the packaged one.
const alternate = process.env.TEMBRA_WASM_BROWSER_ENGINE;
const override = alternate ? path.resolve(alternate) : path.join(enginePackage, "dist/interpreter");
const published: string[] = (await Bun.file(path.join(enginePackage, "package.json")).json()).files;

/** Map a file resolved through the engine package exports to its served URL path. */
function served(file: string): string {
  const relative = path.relative(enginePackage, realpathSync(file));
  if (relative.startsWith("..") || path.isAbsolute(relative))
    throw new Error(`Engine export resolved outside its package: ${file}`);
  return enginePrefix + relative.split(path.sep).join("/");
}

/** Accept only files that the published engine package contains. */
function packaged(relative: string): boolean {
  return published.some((entry) => relative === entry || relative.startsWith(`${entry}/`));
}

// The default asset must be fetched from the same file that the raw `./interpreter` export names.
const defaultEngine = served(
  Bun.resolveSync("@syntropika/tembra-engine-wasm/interpreter", sdkPackage),
);
const overrideEngine = "/engine/interpreter.wasm";
const sdk = await Bun.build({
  entrypoints: [path.join(sdkPackage, "src/browser.ts")],
  target: "browser",
  format: "esm",
  plugins: [
    {
      name: "engine-package",
      setup(builder) {
        // Act like an import map: keep engine modules external and unmodified, so the locator
        // resolves the interpreter from its own served module URL.
        builder.onResolve({ filter: /^@syntropika\/tembra-engine-wasm(?:\/.*)?$/ }, (args) => ({
          path: served(Bun.resolveSync(args.path, sdkPackage)),
          external: true,
        }));
      },
    },
  ],
});
const fixture = await Bun.build({
  entrypoints: [path.join(sdkPackage, "test/interpreter-browser.ts")],
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
// The full suite qualifies an alternate engine when one is supplied; the other path gets a smoke run.
const page = `<!doctype html><body>running<script type="module">
import { verifyBrowserInterpreter, verifyEngineSelection } from "/fixture.js";
try {
  const override = new URL(${JSON.stringify(overrideEngine)}, location.href);
  const alternate = ${JSON.stringify(Boolean(alternate))};
  await verifyBrowserInterpreter(alternate ? override : undefined);
  await verifyEngineSelection(alternate ? undefined : override);
  document.body.textContent = "TEMBRA_WASM_PASS";
} catch (error) {
  document.body.textContent = "TEMBRA_WASM_FAIL " + (error.stack ?? error);
}
</script>`;
const requested = new Set<string>();
const server = Bun.serve({
  hostname: "127.0.0.1",
  port: 0,
  async fetch(request) {
    const pathname = new URL(request.url).pathname;
    requested.add(pathname);
    if (pathname === "/") return new Response(page, { headers: { "Content-Type": "text/html" } });
    if (sources.has(pathname))
      return new Response(sources.get(pathname), {
        headers: { "Content-Type": "application/javascript" },
      });
    if (/^\/[\w-]+\.wasm$/.test(pathname))
      return new Response(Bun.file(path.join(fixtures, pathname.slice(1))));
    let base: string;
    let relative: string;
    if (pathname.startsWith("/engine/")) {
      base = override;
      relative = pathname.slice("/engine/".length);
    } else if (pathname.startsWith(enginePrefix)) {
      base = enginePackage;
      relative = pathname.slice(enginePrefix.length);
      if (!packaged(relative)) return new Response("Not published", { status: 404 });
    } else return new Response("Not found", { status: 404 });
    const resolved = path.resolve(base, relative);
    if (!resolved.startsWith(`${base}${path.sep}`))
      return new Response("Not found", { status: 404 });
    const file = Bun.file(resolved);
    return (await file.exists()) ? new Response(file) : new Response("Not found", { status: 404 });
  },
});
try {
  await headless(server.url.href, "TEMBRA_WASM_PASS", "TEMBRA_WASM_FAIL");
  for (const engine of [defaultEngine, overrideEngine])
    if (!requested.has(engine)) throw new Error(`The browser did not request ${engine}`);
  console.log(
    `Actual browser interpreter facade, memory filesystem and OPFS acceptance passed on the ${alternate ? "alternate" : "default packaged"} engine; the ${alternate ? "default packaged" : "explicit override"} engine path passed its smoke check`,
  );
} finally {
  server.stop(true);
}
