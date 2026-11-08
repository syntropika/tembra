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
for (const entry of entrypoints) {
  const result = await Bun.build({
    entrypoints: [path.join(root, "src", `${entry}.ts`)],
    outdir: path.join(root, "dist", path.dirname(entry)),
    naming: `${path.basename(entry)}.js`,
    target: entry === "node" || entry.startsWith("tooling") ? "node" : "browser",
    packages: "external",
    external: ["#tembra-adapters"],
    splitting: entry === "browser",
    minify: false,
    format: "esm",
  });
  if (!result.success) throw new AggregateError(result.logs, `Failed to build ${entry}`);
}
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
