import { createHash } from "node:crypto";
import { execFileSync, spawnSync } from "node:child_process";
import { copyFile, lstat, mkdir, readFile, readdir, readlink, writeFile } from "node:fs/promises";
import path from "node:path";
import { parseArgs } from "node:util";
import ts from "typescript";
import { checkEngine, prepareBrowserEngine } from "./check-engine.ts";
import * as abi from "../../../packages/ts/sdk/src/interpreter/abi.ts";
import {
  METHODS,
  RESOURCES,
  TYPES,
  WRITABLE_FUTURES,
} from "../../../packages/ts/sdk/src/interpreter/standard.ts";

const revision = "b51e92c0c7ca3b0ae7afff1a1f2de8baf2e7a34a";
const root = import.meta.dir;
const { values } = parseArgs({
  options: {
    sdk: { type: "string" },
    cache: { type: "string" },
    source: { type: "string" },
    toolchain: { type: "string", default: "nightly" },
    package: { type: "boolean", default: false },
  },
});
if (!values.sdk || !values.cache)
  throw new Error("Supply --sdk and --cache; optionally --source and --package");
const cache = path.resolve(values.cache);
const sdk = path.resolve(values.sdk);
await mkdir(cache, { recursive: true });
const checkout = path.resolve(values.source ?? path.join(cache, "checkout"));
if (
  !(await Bun.file(path.join(checkout, "HEAD")).exists()) &&
  !(await exists(path.join(checkout, ".git")))
) {
  if (values.source) throw new Error("The supplied source is not a Git checkout");
  run("git", ["clone", "https://github.com/stencil-hq/dquickts", checkout]);
}
const archive = execFileSync("git", ["-C", checkout, "archive", revision], { maxBuffer: 64 << 20 });
const source = path.join(cache, `dquickts-${revision}`);
if (!(await exists(source))) {
  await mkdir(source);
  const archivePath = path.join(cache, "source.tar");
  await writeFile(archivePath, archive);
  run("tar", ["-xf", archivePath, "-C", source]);
  const dtoa = path.join(source, "dtoa.c");
  const text = await readFile(dtoa, "utf8");
  if (text.split("#include <setjmp.h>").length !== 2)
    throw new Error("Pinned dtoa include changed");
  await writeFile(
    dtoa,
    text.replace("#include <setjmp.h>", "#if !defined(__wasi__)\n#include <setjmp.h>\n#endif"),
  );
}
await verifySource(checkout, source);
const bootstrap = path.join(cache, "bootstrap");
const config = ts.readConfigFile(path.join(root, "tsconfig.json"), ts.sys.readFile);
const parsed = ts.parseJsonConfigFileContent(config.config, ts.sys, root);
const program = ts.createProgram(parsed.fileNames, parsed.options);
const diagnostics = ts.getPreEmitDiagnostics(program);
if (diagnostics.length)
  throw new Error(
    ts.formatDiagnosticsWithColorAndContext(diagnostics, {
      getCurrentDirectory: () => root,
      getCanonicalFileName: (name) => name,
      getNewLine: () => "\n",
    }),
  );
const bundled = await Bun.build({
  entrypoints: ["bridge", "state", "values", "standard", "runtime"].map((name) =>
    path.join(root, "src", `${name}.ts`),
  ),
  outdir: bootstrap,
  format: "iife",
  target: "browser",
  // Parse generated metadata as data. Compiling its object/array literals as JavaScript in
  // every Orb spends fuel on bytecode that only reconstructs this immutable schema.
  plugins: [
    {
      name: "standard-schema-data",
      setup(builder) {
        const table = path.resolve(root, "../../../packages/ts/sdk/src/interpreter/standard.ts");
        builder.onResolve({ filter: /standard\.ts$/ }, (request) => {
          if (path.resolve(request.resolveDir, request.path) === table)
            return { path: "schema", namespace: "standard-schema-data" };
        });
        builder.onLoad({ filter: /.*/, namespace: "standard-schema-data" }, () => ({
          contents: `export const {METHODS, RESOURCES, TYPES, WRITABLE_FUTURES} = JSON.parse(${JSON.stringify(JSON.stringify({ METHODS, RESOURCES, TYPES, WRITABLE_FUTURES }))});`,
          loader: "js",
        }));
      },
    },
  ],
  minify: { whitespace: true, syntax: true, identifiers: true },
});
if (!bundled.success) throw new AggregateError(bundled.logs, "Guest bootstrap build failed");
// Rust requires literal link attributes; keep their names derived from the same ABI constants.
await writeFile(
  path.join(bootstrap, "host-imports.rs"),
  `#[link(wasm_import_module = ${JSON.stringify(abi.IMPORT_MODULE)})]
unsafe extern "C" {
    #[link_name = ${JSON.stringify(abi.SYNC_IMPORT)}]
    pub fn sync(packet: *const u8, length: usize) -> u32;
    #[link_name = ${JSON.stringify(abi.SYNC_READ_IMPORT)}]
    pub fn sync_read(destination: *mut u8, length: usize);
}
`,
);
// The interpreter is a WASI preview 1 reactor core module. Its linear memory is imported so each
// host instance supplies the configured maximum; the initial size is part of the private ABI.
const target = "wasm32-wasip1";
const sysroot = path.join(sdk, "share/wasi-sysroot");
const env = {
  ...process.env,
  WASI_SDK: sdk,
  RQUICKJS_SYS_NO_WASI_SDK: "1",
  CC_wasm32_wasip1: path.join(sdk, "bin/clang"),
  AR_wasm32_wasip1: path.join(sdk, "bin/llvm-ar"),
  CFLAGS_wasm32_wasip1: `--sysroot=${sysroot}`,
  BINDGEN_EXTRA_CLANG_ARGS_wasm32_wasip1: `--sysroot=${sysroot} -isystem ${path.join(sysroot, `include/${target}`)}`,
  CARGO_TARGET_DIR: path.join(cache, "target"),
  TEMBRA_GUEST_BOOTSTRAP_DIR: bootstrap,
  TEMBRA_INTERPRETER_IMPORT_MODULE: abi.IMPORT_MODULE,
};
const patch = `patch."https://github.com/stencil-hq/dquickts".rquickjs.path=${JSON.stringify(path.join(source, "rquickjs"))}`;
const link = [
  "-Clink-arg=--import-memory",
  `-Clink-arg=--initial-memory=${abi.MEMORY_INITIAL_PAGES * abi.PAGE_BYTES}`,
];
run(
  "cargo",
  [
    `+${values.toolchain}`,
    "build",
    "--locked",
    "--release",
    "--target",
    target,
    "--manifest-path",
    path.join(root, "Cargo.toml"),
    "--config",
    patch,
    "--config",
    `target.${target}.rustflags=${JSON.stringify(link)}`,
  ],
  env,
);
const destination = path.join(cache, "engine.wasm");
await copyFile(path.join(cache, `target/${target}/release/tembra_dquickts.wasm`), destination);
const bytes = await readFile(destination);
await checkEngine(bytes);
const manifest = {
  engine: "dquickts",
  protocol: `${abi.IMPORT_MODULE} (wasm32-wasip1 core reactor)`,
  revision,
  sourceArchiveSha256: createHash("sha256").update(archive).digest("hex"),
  wasmSha256: createHash("sha256").update(bytes).digest("hex"),
  bytes: bytes.length,
  bun: Bun.version,
  rust: execFileSync("rustc", [`+${values.toolchain}`, "-Vv"], { encoding: "utf8" }),
  sdk: execFileSync(path.join(sdk, "bin/clang"), ["--version"], { encoding: "utf8" }),
};
await writeFile(path.join(cache, "engine.json"), JSON.stringify(manifest, null, 2) + "\n");
if (values.package) {
  const packages = path.resolve(root, "../../../packages/ts");
  await mkdir(path.join(packages, "engine-node/dist"), { recursive: true });
  await copyFile(destination, path.join(packages, "engine-node/dist/interpreter.wasm"));
  await prepareBrowserEngine(bytes, path.join(packages, "engine-wasm/dist/interpreter"));
}
console.log(JSON.stringify({ module: destination, ...manifest }));

/** Stop the build when a tool fails, retaining the compiler's own diagnostic. */
function run(command: string, args: string[], env = process.env): void {
  const result = spawnSync(command, args, { env, stdio: "inherit" });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`${command} exited with ${result.status}`);
}

/** Distinguish a missing owned path from other filesystem failures. */
async function exists(file: string): Promise<boolean> {
  try {
    await lstat(file);
    return true;
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return false;
    throw error;
  }
}

/** Verify every cached byte and symlink against the pinned Git tree and the one WASI include patch. */
async function verifySource(checkout: string, directory: string): Promise<void> {
  const tree = execFileSync("git", ["-C", checkout, "ls-tree", "-rz", "--full-tree", revision], {
    encoding: "utf8",
    maxBuffer: 8 << 20,
  });
  const expected = new Set<string>();
  for (const entry of tree.split("\0").filter(Boolean)) {
    const match = /^(\d+) blob ([0-9a-f]{40})\t([\s\S]+)$/.exec(entry);
    if (!match) throw new Error("Unsupported pinned source entry");
    const mode = match[1]!;
    const hash = match[2]!;
    const relative = match[3]!;
    const file = path.resolve(directory, relative);
    if (!file.startsWith(directory + path.sep))
      throw new Error("Pinned source escaped its directory");
    const stat = await lstat(file);
    if (stat.isSymbolicLink() !== (mode === "120000"))
      throw new Error(`Cached source type differs: ${relative}`);
    let data = mode === "120000" ? Buffer.from(await readlink(file)) : await readFile(file);
    if (relative === "dtoa.c")
      data = Buffer.from(
        data
          .toString()
          .replace("#if !defined(__wasi__)\n#include <setjmp.h>\n#endif", "#include <setjmp.h>"),
      );
    const actual = createHash("sha1").update(`blob ${data.length}\0`).update(data).digest("hex");
    if (actual !== hash) throw new Error(`Cached source differs: ${relative}`);
    expected.add(relative);
  }
  /** Refuse untracked files without following cached symlinks. */
  async function visit(current: string): Promise<void> {
    for (const item of await readdir(current, { withFileTypes: true })) {
      const file = path.join(current, item.name);
      if (item.isDirectory()) await visit(file);
      else if (!expected.delete(path.relative(directory, file)))
        throw new Error("Unexpected cached source file");
    }
  }
  await visit(directory);
  if (expected.size) throw new Error("Cached source is incomplete");
}
