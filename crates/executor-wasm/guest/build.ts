import { createHash } from "node:crypto";
import { execFileSync, spawnSync } from "node:child_process";
import {
  copyFile,
  lstat,
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  readlink,
  rename,
  rm,
  writeFile,
} from "node:fs/promises";
import path from "node:path";
import { homedir } from "node:os";
import { createRequire } from "node:module";
import { parseArgs } from "node:util";

// Editing an Orb reuses this dylib. Only a runtime or toolchain change rebuilds it.
const quickjsRevision = "b51e92c0c7ca3b0ae7afff1a1f2de8baf2e7a34a";
const runtimeRevision = "758687536ea711ede17369f53a90ddc3e97a8dbb";
const root = import.meta.dir;
const { values } = parseArgs({
  options: {
    sdk: { type: "string" },
    cache: { type: "string" },
    "quickjs-source": { type: "string" },
    "runtime-source": { type: "string" },
    toolchain: { type: "string", default: "1.99.0" },
    package: { type: "boolean", default: false },
  },
});
if (!values.sdk || !values.cache) throw new Error("Supply --sdk and --cache");
const cache = path.resolve(values.cache);
const sdk = path.resolve(values.sdk);
await mkdir(cache, { recursive: true });
const runtimePatch = path.join(root, "patches/runtime.patch");
const runtimePatchBytes = await readFile(runtimePatch);
const patchHash = digest(runtimePatchBytes);
const enginePatchBytes = await readFile(path.join(root, "patches/dquickts.patch"));
const enginePatchHash = digest(enginePatchBytes);
// Cargo hashes a path package relative to the workspace root only when it lies inside that
// root; otherwise its absolute location changes crate identities, TypeIds and code layout.
// The patched engine therefore lives at a fixed path inside the runtime workspace.
const engineDirectory = "tembra-dquickts";
const quickjsArchive = await archive(
  "dquickts",
  "https://github.com/stencil-hq/dquickts",
  quickjsRevision,
  values["quickjs-source"],
);
const runtimeArchive = await archive(
  "componentize-qjs",
  "https://github.com/andreiltd/componentize-qjs",
  runtimeRevision,
  values["runtime-source"],
);
const runtime = await source(
  `componentize-qjs-${runtimeRevision}-dquickts-${quickjsRevision}-${digest(Buffer.from(`${patchHash}\0${enginePatchHash}`))}`,
  async (directory, scratch) => {
    await extract(runtimeArchive, directory, scratch);
    await applyPatch(runtimePatchBytes, directory, scratch, "runtime.patch");
    // The pinned lockfile stays authoritative. Only the four registry entries replaced by
    // the pinned local dquickts workspace lose their registry identity; no resolution runs.
    const file = path.join(directory, "Cargo.lock");
    const lock = await readFile(file, "utf8");
    let replaced = 0;
    const patched = lock.replace(
      /(\[\[package\]\]\nname = "rquickjs(?:-core|-macro|-sys)?"\nversion = "0\.14\.0"\n)source = "registry\+https:\/\/github.com\/rust-lang\/crates.io-index"\nchecksum = "[0-9a-f]+"\n/g,
      (_match, header: string) => {
        replaced++;
        return header;
      },
    );
    if (replaced !== 4) throw new Error("Pinned QuickJS lock entries changed");
    await writeFile(file, patched);
    const engine = path.join(directory, engineDirectory);
    if (await exists(engine)) throw new Error(`Runtime source already contains ${engineDirectory}`);
    await mkdir(engine);
    await extract(quickjsArchive, engine, scratch);
    await applyPatch(enginePatchBytes, engine, scratch, "dquickts.patch", ["--fuzz=0"]);
  },
);
const quickjs = path.join(runtime, engineDirectory);
const target = "wasm32-wasip2";
const sysroot = path.join(sdk, "share/wasi-sysroot");
// Source and build-output locations can enter panic strings and change data offsets. Keep the
// shared core identical across build machines, cache roots and dependency-cache locations.
// rustc applies the last matching prefix, so nested locations follow their parents.
const remaps = [
  [cache, "/tembra"],
  [process.env.CARGO_HOME ?? path.join(homedir(), ".cargo"), "/cargo"],
  [sdk, "/wasi-sdk"],
] as const;
const rustFlags = [
  "-Clink-arg=-shared",
  "-Clink-arg=-Wl,--no-entry",
  "-Clink-arg=-Wl,--allow-undefined",
  "-Clto=fat",
  "-Copt-level=3",
  ...remaps.map(([from, to]) => `--remap-path-prefix=${from}=${to}`),
];
const env: NodeJS.ProcessEnv = {
  ...process.env,
  WASI_SDK: sdk,
  WASI_SDK_PATH: sdk,
  RQUICKJS_SYS_NO_WASI_SDK: "1",
  CC_wasm32_wasip2: path.join(sdk, "bin/clang"),
  AR_wasm32_wasip2: path.join(sdk, "bin/llvm-ar"),
  CFLAGS_wasm32_wasip2: `-fPIC -O3 --sysroot=${sysroot} ${remaps.map(([from, to]) => `-ffile-prefix-map=${from}=${to}`).join(" ")}`,
  BINDGEN_EXTRA_CLANG_ARGS_wasm32_wasip2: `--sysroot=${sysroot} -isystem ${path.join(sysroot, `include/${target}`)}`,
  CARGO_TARGET_DIR: path.join(cache, "target"),
  CARGO_TARGET_WASM32_WASIP2_LINKER: path.join(sdk, "bin/clang"),
  CARGO_ENCODED_RUSTFLAGS: rustFlags.join("\x1f"),
  CARGO_PROFILE_RELEASE_CODEGEN_UNITS: "1",
  CARGO_INCREMENTAL: "0",
};
// An encoded list preserves paths containing spaces and overrides inherited target flags.
delete env.CARGO_TARGET_WASM32_WASIP2_RUSTFLAGS;
run(
  "rustup",
  [
    "run",
    values.toolchain ?? "1.99.0",
    "cargo",
    "build",
    "--locked",
    "--release",
    "--target",
    target,
    "--package",
    "componentize-qjs-runtime",
    "--no-default-features",
    "--features",
    "component-model-async",
    "--config",
    `patch.crates-io.rquickjs.path=${JSON.stringify(path.join(quickjs, "rquickjs"))}`,
  ],
  env,
  runtime,
);
const destination = path.join(cache, "runtime.wasm");
const bytes = await readFile(
  path.join(cache, `target/${target}/release/componentize_qjs_runtime.wasm`),
);
// This is a core dylib consumed by wit-dylib, not a separately installed Orb or host engine.
const module = await WebAssembly.compile(bytes);
const exports = new Set(WebAssembly.Module.exports(module).map((item) => item.name));
for (const name of [
  "init",
  "wit_dylib_initialize",
  "wit_dylib_export_async_call",
  "wit_dylib_pop_u64",
])
  if (!exports.has(name)) throw new Error(`Runtime dylib export is missing: ${name}`);
await writeFile(destination, bytes);
const manifest = {
  engine: "dquickts",
  quickjsRevision,
  runtimeRevision,
  patchSha256: patchHash,
  enginePatchSha256: enginePatchHash,
  wasmSha256: digest(bytes),
  cargo: execFileSync("rustup", ["run", values.toolchain ?? "1.99.0", "cargo", "-V"], {
    encoding: "utf8",
  }),
  rust: execFileSync("rustc", [`+${values.toolchain}`, "-Vv"], {
    encoding: "utf8",
  }),
  sdk: execFileSync(path.join(sdk, "bin/clang"), ["--version"], {
    encoding: "utf8",
  }),
};
await writeFile(path.join(cache, "runtime.json"), JSON.stringify(manifest, null, 2) + "\n");
let core: string | undefined;
if (values.package) {
  // Every Engine links the core that the real componentizer derives from this dylib. Package a
  // dylib only when that derivation equals the pinned core; a runtime change replaces both.
  const binding = path.resolve(root, "../../../packages/ts/engine-node/index.cjs");
  const { NodeContract } = createRequire(import.meta.url)(binding);
  if (typeof NodeContract?.sourceRuntime !== "function")
    throw new Error(`Build the Node binding before packaging the runtime: ${binding}`);
  const derived: Uint8Array = NodeContract.sourceRuntime(bytes);
  const candidate = path.join(cache, "dquickts.wasm");
  await writeFile(candidate, derived);
  const pinned = path.resolve(root, "../../contracts/runtime/dquickts.wasm");
  const expected = await readFile(pinned);
  if (!expected.equals(derived))
    throw new Error(
      `Derived runtime core ${digest(derived)} differs from pinned ${digest(expected)} at ${pinned}; ` +
        `derived core: ${candidate}`,
    );
  core = digest(derived);
  const output = path.resolve(root, "../../../packages/ts/engine-node/dist");
  await mkdir(output, { recursive: true });
  await copyFile(destination, path.join(output, "runtime.wasm"));
}
console.log(JSON.stringify({ module: destination, ...manifest, coreSha256: core }));

/** Archive only pinned Git objects; supplied working-tree edits never become runtime inputs. */
async function archive(
  name: string,
  repository: string,
  revision: string,
  supplied: string | undefined,
): Promise<Buffer> {
  const checkout = path.resolve(supplied ?? path.join(cache, `${name}-checkout`));
  if (!(await exists(checkout))) {
    if (supplied) throw new Error(`Missing ${name} source checkout`);
    run("git", ["clone", "--no-checkout", "--filter=blob:none", repository, checkout]);
  }
  return execFileSync("git", ["-C", checkout, "archive", revision], { maxBuffer: 64 << 20 });
}

/** Reconstruct the expected tree on every build, so cache edits and unexpected files are
 * rejected. Scratch ownership is local to this invocation; existing caches are never deleted. */
async function source(
  name: string,
  populate: (directory: string, scratch: string) => Promise<void>,
): Promise<string> {
  const directory = path.join(cache, name);
  const scratch = await mkdtemp(path.join(cache, ".prepare-"));
  try {
    const expected = path.join(scratch, "source");
    await mkdir(expected);
    await populate(expected, scratch);
    if (await exists(directory)) {
      if (!(await lstat(directory)).isDirectory())
        throw new Error(`Cached source must be a directory: ${directory}`);
      if ((await treeDigest(directory)) !== (await treeDigest(expected)))
        throw new Error(`Cached source differs from its pinned inputs: ${directory}`);
    } else {
      await rename(expected, directory);
    }
    return directory;
  } finally {
    await rm(scratch, { recursive: true, force: true });
  }
}

async function extract(bytes: Buffer, directory: string, scratch: string): Promise<void> {
  const tar = path.join(scratch, "source.tar");
  await writeFile(tar, bytes);
  run("tar", ["-xf", tar, "-C", directory]);
  await rm(tar);
}

async function applyPatch(
  bytes: Buffer,
  directory: string,
  scratch: string,
  name: string,
  options: string[] = [],
): Promise<void> {
  const file = path.join(scratch, name);
  await writeFile(file, bytes);
  run("patch", ["--batch", "--forward", ...options, "-p1", "-i", file], undefined, directory);
}

/** Hash file names, modes, symlink targets and bytes without following cached symlinks. */
async function treeDigest(directory: string): Promise<string> {
  const hash = createHash("sha256");
  async function visit(current: string): Promise<void> {
    for (const item of (await readdir(current, { withFileTypes: true })).sort((a, b) =>
      a.name.localeCompare(b.name),
    )) {
      const file = path.join(current, item.name);
      const relative = path.relative(directory, file);
      const stat = await lstat(file);
      if (stat.isDirectory()) {
        await visit(file);
        continue;
      }
      if (!stat.isFile() && !stat.isSymbolicLink())
        throw new Error(`Unsupported source entry: ${file}`);
      const bytes = stat.isSymbolicLink()
        ? Buffer.from(await readlink(file))
        : await readFile(file);
      hash.update(
        `${relative}\0${stat.isSymbolicLink() ? "link" : stat.mode & 0o111}\0${bytes.length}\0`,
      );
      hash.update(bytes);
    }
  }
  await visit(directory);
  return hash.digest("hex");
}

function digest(bytes: Uint8Array): string {
  return createHash("sha256").update(bytes).digest("hex");
}

function run(command: string, args: string[], env = process.env, cwd?: string): void {
  const result = spawnSync(command, args, { env, cwd, stdio: "inherit" });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`${command} exited with ${result.status}`);
}

async function exists(file: string): Promise<boolean> {
  try {
    await lstat(file);
    return true;
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return false;
    throw error;
  }
}
