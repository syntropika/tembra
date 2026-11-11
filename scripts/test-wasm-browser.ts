import { existsSync, realpathSync } from "node:fs";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
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
/** Bundle one test entry; its SDK import stays the page's single shared `/sdk.js`. */
const testEntry = (file: string) =>
  Bun.build({
    entrypoints: [path.join(sdkPackage, "test", file)],
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
const fixture = await testEntry("interpreter-browser.ts");
const prepared = await testEntry("prepared-component-browser.ts");
if (!sdk.success || !fixture.success || !prepared.success)
  throw new AggregateError(
    [...sdk.logs, ...fixture.logs, ...prepared.logs],
    "Browser fixture build failed",
  );
const sources = new Map([
  ["/sdk.js", await sdk.outputs[0]!.text()],
  ["/fixture.js", await fixture.outputs[0]!.text()],
  ["/prepared.js", await prepared.outputs[0]!.text()],
]);
// Compiled fixtures whose generated browser adapters the prepared-component page imports.
const adapters = [
  "arithmetic",
  "pipeline",
  "probe",
  "hook-compiled",
  "http-client",
  "http-handler",
];
for (const name of adapters)
  if (!(await Bun.file(path.join(fixtures, name, "browser/index.js")).exists()))
    throw new Error(`Fixture ${name} has no prepared browser adapter; rebuild the fixtures`);
// The full suite qualifies an alternate engine when one is supplied; the other path gets a smoke run.
const page = `<!doctype html><body>running<script type="module">
import { verifyBrowserInterpreter, verifyEngineSelection, verifyStandardScenario, verifyPrecisionBrowser, verifyContextAcquireBrowser, verifyIngressScenario } from "/fixture.js";
try {
  const override = new URL(${JSON.stringify(overrideEngine)}, location.href);
  const alternate = ${JSON.stringify(Boolean(alternate))};
  await verifyBrowserInterpreter(alternate ? override : undefined);
  await verifyEngineSelection(alternate ? undefined : override);
  const standard = await verifyStandardScenario(alternate ? override : undefined);
  await verifyPrecisionBrowser(alternate ? override : undefined);
  await verifyContextAcquireBrowser(alternate ? override : undefined);
  const ingress = await verifyIngressScenario(alternate ? override : undefined);
  await fetch("/report", { method: "POST", body: JSON.stringify({ standard, ingress }) });
  document.body.textContent = "TEMBRA_WASM_PASS";
} catch (error) {
  document.body.textContent = "TEMBRA_WASM_FAIL " + (error.stack ?? error);
}
</script>`;
// Compiled Orbs run through application-owned adapters. The import map resolves each generated
// module's bare SDK import to the same `/sdk.js` the application imports, so both share one SDK
// instance and therefore one prepared-component registry brand.
const preparedPage = `<!doctype html><script type="importmap">${JSON.stringify({
  imports: { "@syntropika/tembra-sdk/browser": "/sdk.js" },
})}</script><body>running<script type="module">
import { verifyPreparedComponents } from "/prepared.js";
try {
  const passed = await verifyPreparedComponents(${JSON.stringify(Boolean(alternate))} ? new URL(${JSON.stringify(overrideEngine)}, location.href) : undefined);
  await fetch("/report/prepared", { method: "POST", body: JSON.stringify(passed) });
  document.body.textContent = "TEMBRA_WASM_PASS";
} catch (error) {
  document.body.textContent = "TEMBRA_WASM_FAIL " + (error.stack ?? error);
}
</script>`;
const requested = new Set<string>();
/** Scenario evidence posted by the page, including actual incoming HTTP lifecycle checks. */
let standardReport: unknown;
/** Names of the prepared-component scenarios the page passed. */
let preparedReport: string[] | undefined;
// Every Chrome profile of this run lives under one run-owned directory, removed at shutdown.
const profiles = await mkdtemp(
  path.join(process.env.TEMBRA_TEST_ARTIFACTS ?? tmpdir(), "tembra-wasm-browser-"),
);
process.env.TEMBRA_TEST_ARTIFACTS = profiles;

/** Remove this run's profiles; a Chrome child can still flush state just after its browser exits. */
async function removeProfiles(): Promise<void> {
  for (let attempt = 0; attempt < 20; attempt++) {
    await rm(profiles, { recursive: true, force: true });
    await Bun.sleep(100);
    if (!existsSync(profiles)) return;
  }
  throw new Error(`Chrome kept writing ${profiles} after shutdown`);
}
const server = Bun.serve({
  hostname: "127.0.0.1",
  port: 0,
  async fetch(request) {
    const pathname = new URL(request.url).pathname;
    requested.add(pathname);
    if (pathname === "/report" && request.method === "POST") {
      standardReport = await request.json();
      return new Response(null, { status: 204 });
    }
    if (pathname === "/report/prepared" && request.method === "POST") {
      preparedReport = await request.json();
      return new Response(null, { status: 204 });
    }
    if (pathname === "/prepared")
      return new Response(preparedPage, { headers: { "Content-Type": "text/html" } });
    const adapterFile = pathname.match(/^\/adapters\/([\w-]+)\/([\w.-]+\.(?:js|wasm))$/);
    if (adapterFile) {
      const [, name, file] = adapterFile;
      if (!adapters.includes(name!)) return new Response("Not found", { status: 404 });
      const served = Bun.file(path.join(fixtures, name!, "browser", file!));
      if (!(await served.exists())) return new Response("Not found", { status: 404 });
      return new Response(served, {
        headers: {
          "Content-Type": file!.endsWith(".js") ? "application/javascript" : "application/wasm",
        },
      });
    }
    if (pathname === "/wasi/values") return Response.json({ values: [2, 4, 6] });
    if (pathname === "/wasi/events")
      return new Response(
        new ReadableStream<Uint8Array>({
          start(controller) {
            // Keep the response open: the guest must read the first event without waiting for EOF.
            controller.enqueue(new TextEncoder().encode("data: browser fetch\n\n"));
          },
        }),
        { headers: { "Content-Type": "text/event-stream" } },
      );
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
  console.log(`Browser WASI and incoming HTTP scenarios: ${JSON.stringify(standardReport)}`);
  console.log(
    `Actual browser interpreter facade, memory filesystem and OPFS acceptance passed on the ${alternate ? "alternate" : "default packaged"} engine; the ${alternate ? "default packaged" : "explicit override"} engine path passed its smoke check`,
  );
  // A separate page and Chrome profile: compiled Orbs through their generated adapters.
  await headless(new URL("/prepared", server.url).href, "TEMBRA_WASM_PASS", "TEMBRA_WASM_FAIL");
  for (const name of adapters)
    for (const file of ["index.js", "glue.js"])
      if (!requested.has(`/adapters/${name}/${file}`))
        throw new Error(`The browser did not import the ${name} adapter's ${file}`);
  // The import map, not a second SDK copy, satisfied every adapter's SDK import.
  if ([...requested].some((pathname) => pathname.includes("tembra-sdk")))
    throw new Error("An adapter requested a second SDK module");
  if (
    ![...requested].some((pathname) =>
      /^\/adapters\/[\w-]+\/orb\.core\.[0-9a-f]+\.wasm$/.test(pathname),
    )
  )
    throw new Error("No prepared core module was fetched");
  if (!preparedReport?.length) throw new Error("The prepared-component page reported nothing");
  console.log(
    `Prepared compiled components in Chrome: ${preparedReport.length} scenarios passed:\n- ${preparedReport.join("\n- ")}`,
  );
} finally {
  await server.stop(true);
  await removeProfiles();
}
