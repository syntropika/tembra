import { copyFile, mkdir, readdir, rm, writeFile } from "node:fs/promises";
import path from "node:path";
import { mkdirSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import ts from "typescript";
import { witTypes } from "./src/tooling/wit.ts";

const root = fileURLToPath(new URL("./", import.meta.url));
// The private guest controls use declarations generated from their canonical WIT as well.
await import("./scripts/guest-wit.ts");
// The library uses the same official declarations as authored Orbs. They are build output,
// not hand-maintained copies of the standard's resource and future signatures.
const standard = { files: new Map<string, string>() };
for (const world of ["http", "sockets"])
  for (const [file, contents] of (await witTypes(path.join(root, `wit/${world}.wit`), world)).files)
    standard.files.set(file, contents);
for (const [file, contents] of standard.files) {
  if (!file.includes(`${path.sep}interfaces${path.sep}`)) continue;
  const destination = path.join(root, ".tembra/wasi/interfaces", path.basename(file));
  await mkdir(path.dirname(destination), { recursive: true });
  await writeFile(destination, contents);
}
const entrypoints = [
  "index",
  "guest",
  "guest/shim",
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
  external: ["node:*", "bun:*", "wasi:*"],
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
// Keep standard types package-private in every public declaration, including provider errors.
// Applications need no virtual WASI module resolution or authoring toolchain.
const standardTypes = new Map<string, string>();
for (const [file, contents] of standard.files) {
  if (!file.includes(`${path.sep}interfaces${path.sep}`)) continue;
  const destination = path.join(root, "dist/wasi", path.basename(file));
  const name = /@module Interface ([^ ]+)/.exec(contents)?.[1];
  if (name) standardTypes.set(name, destination.replace(/\.d\.ts$/, ".js"));
  await mkdir(path.dirname(destination), { recursive: true });
  await writeFile(destination, contents);
}
const emitted = declarations.emit(undefined, (file, contents) => {
  const rewritten = contents.replace(
    /(\bfrom\s+|\bimport\s*\(\s*)(["'])(wasi:[^"']+)\2/g,
    (_match, prefix, quote, name) => {
      const target = standardTypes.get(name);
      if (!target) throw new Error(`No packaged declaration for ${name}`);
      const relative = path.relative(path.dirname(file), target).split(path.sep).join("/");
      return `${prefix}${quote}${relative.startsWith(".") ? relative : `./${relative}`}${quote}`;
    },
  );
  mkdirSync(path.dirname(file), { recursive: true });
  writeFileSync(file, rewritten);
});
if (emitted.emitSkipped) throw new Error("Declaration output failed");

// TypeScript does not copy input declaration files when emitting declarations. Retain the
// private WIT types beside the internal guest shim so the packaged graph remains complete.
const guestTypes = path.join(root, "src/guest/.tembra");
const guestDestination = path.join(root, "dist/guest/.tembra");
await mkdir(guestDestination, { recursive: true });
for (const file of await readdir(guestTypes))
  if (file.endsWith(".d.ts"))
    await copyFile(path.join(guestTypes, file), path.join(guestDestination, file));
