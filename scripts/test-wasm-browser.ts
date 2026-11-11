import { realpathSync } from "node:fs";
import { NodeContract } from "@syntropika/tembra-engine-node";
import path from "node:path";
import { headless } from "../packages/ts/test/headless.ts";

const fixtures = process.env.TEMBRA_WASM_FIXTURES;
if (!fixtures) throw new Error("Set TEMBRA_WASM_FIXTURES to the built qualification artifacts");
const root = path.resolve(import.meta.dir, "..");
const probeFile = process.env.TEMBRA_WASM_PROBE;
if (!probeFile) throw new Error("Set TEMBRA_WASM_PROBE to the built compiled Rust probe");
const compiledProbe = new NodeContract(
  path.join(root, "crates/executor-wasm/tests/wit/probe"),
  "probe-provider",
).componentArtifact(new Uint8Array(await Bun.file(probeFile).arrayBuffer()), "probe", 1);
const sdkPackage = path.join(root, "packages/ts/sdk");
const enginePackage = realpathSync(path.join(root, "packages/ts/engine-wasm"));
const enginePrefix = "/packages/ts/engine-wasm/";
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

const sdk = await Bun.build({
  entrypoints: [path.join(sdkPackage, "src/browser.ts")],
  target: "browser",
  format: "esm",
  plugins: [
    {
      name: "engine-package",
      setup(builder) {
        // Serve the published kernel, verifier and trusted compiler from their package URLs.
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
const scenarios = [
  "verifyBrowserInterpreter",
  "verifyArithmeticSource",
  "verifySourceCpuLimit",
  "verifyCompiledLimits",
  "verifyStandardScenario",
  "verifyPrecisionBrowser",
  "verifyContextAcquireBrowser",
  "verifyDetachedWorkRefusal",
  "verifyIngressScenario",
] as const;
const runner = `
import * as scenarios from "/fixture.js";
try {
  const scenario = new URL(location.href).searchParams.get("scenario");
  if (!scenario || typeof scenarios[scenario] !== "function") throw new Error("Unknown browser scenario");
  const result = await scenarios[scenario]();
  await fetch("/report", { method: "POST", body: JSON.stringify({ scenario, result: result ?? null }) });
  document.body.textContent = "TEMBRA_WASM_PASS";
} catch (error) {
  document.body.textContent = "TEMBRA_WASM_FAIL " + (error.stack ?? error);
}
`;
sources.set("/runner.js", runner);
const page = '<!doctype html><body>running<script type="module" src="/runner.js"></script>';
const requested = new Set<string>();
/** Scenario evidence posted by the page, including actual incoming HTTP lifecycle checks. */
const reports: Record<string, unknown> = {};
const server = Bun.serve({
  hostname: "127.0.0.1",
  port: 0,
  async fetch(request) {
    const pathname = new URL(request.url).pathname;
    requested.add(pathname);
    if (pathname === "/report" && request.method === "POST") {
      const report = (await request.json()) as { scenario: string; result: unknown };
      reports[report.scenario] = report.result;
      return new Response(null, { status: 204 });
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
    if (pathname === "/")
      return new Response(page, {
        headers: {
          "Content-Type": "text/html",
          "Content-Security-Policy":
            "default-src 'self'; script-src 'self' blob: 'wasm-unsafe-eval'; object-src 'none'; base-uri 'none'",
        },
      });
    if (sources.has(pathname))
      return new Response(sources.get(pathname), {
        headers: { "Content-Type": "application/javascript" },
      });
    if (pathname === "/compiled-probe.wasm") return new Response(Uint8Array.from(compiledProbe));
    if (/^\/[\w-]+\.wasm$/.test(pathname))
      return new Response(Bun.file(path.join(fixtures, pathname.slice(1))));
    let base: string;
    let relative: string;
    if (pathname.startsWith(enginePrefix)) {
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
  for (const scenario of scenarios) {
    const url = new URL(server.url);
    url.searchParams.set("scenario", scenario);
    await headless(url.href, "TEMBRA_WASM_PASS", "TEMBRA_WASM_FAIL");
    if (!Object.hasOwn(reports, scenario))
      throw new Error(`The browser did not report ${scenario}`);
  }
  for (const asset of [
    "dist/compiler.js",
    "dist/compiler/js-component-bindgen-component.core.wasm",
    "dist/compiler/js-component-bindgen-component.core2.wasm",
  ])
    if (!requested.has(enginePrefix + asset))
      throw new Error(`The browser did not request ${asset}`);
  console.log(`Browser WASI and incoming HTTP scenarios: ${JSON.stringify(reports)}`);
  console.log("Component browser execution, MemoryFS, OPFS and WASI acceptance passed");
} finally {
  server.stop(true);
}
