import { mkdir, rm } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import ts from "typescript";

const root = fileURLToPath(new URL("./", import.meta.url));
const entrypoints = [
  "index",
  "guest",
  "host",
  "http",
  "runtime/adapters",
  "node",
  "browser",
  "tooling/index",
  "msgpack",
  "coordinator",
  "manager",
  "rpc",
  "telemetry",
];
await rm(path.join(root, "dist"), { recursive: true, force: true });
await mkdir(path.join(root, "dist"), { recursive: true });
// Build one module graph so classes and adapter state have one identity across entrypoints.
const result = await Bun.build({
  entrypoints: entrypoints.map((entry) => path.join(root, "src", `${entry}.ts`)),
  root: path.join(root, "src"),
  outdir: path.join(root, "dist"),
  naming: { entry: "[dir]/[name].js", chunk: "chunks/[name]-[hash].js" },
  target: "browser",
  packages: "external",
  external: ["node:*", "bun:*"],
  conditions: ["source"],
  plugins: [
    {
      name: "shared-adapters",
      /** Keep private adapter state in the same compiled graph as its callers. */
      setup(builder) {
        builder.onResolve({ filter: /^#tembra-adapters$/ }, () => ({
          path: path.join(root, "src/runtime/adapters.ts"),
        }));
      },
    },
  ],
  splitting: true,
  // Consuming applications choose preloads; library chunk paths change when rebundled.
  modulePreload: false,
  minify: false,
  format: "esm",
});
if (!result.success) throw new AggregateError(result.logs, "SDK build failed");
const config = ts.readConfigFile(path.join(root, "tsconfig.json"), ts.sys.readFile);
const parsed = ts.parseJsonConfigFileContent(
  { ...config.config, include: ["src/**/*.ts"] },
  ts.sys,
  root,
);
const declarations = ts.createProgram(parsed.fileNames, {
  ...parsed.options,
  noEmit: false,
  declaration: true,
  emitDeclarationOnly: true,
  rootDir: path.join(root, "src"),
  outDir: path.join(root, "dist"),
});
const diagnostics = ts.getPreEmitDiagnostics(declarations);
if (diagnostics.length)
  throw new Error(
    ts.formatDiagnosticsWithColorAndContext(diagnostics, {
      getCurrentDirectory: () => root,
      getCanonicalFileName: (name) => name,
      getNewLine: () => "\n",
    }),
  );
if (declarations.emit().emitSkipped) throw new Error("Declaration output failed");
