// Rebuild the engine with the matched patched generator and runtime.
import { createWorld } from "@bytecodealliance/jco-transpile";
import { validateBootstrap } from "./validate-bootstrap.mjs";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";

const here = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const generator = process.env.COMPONENTIZE_QJS ?? `${here}/.cache/toolchain/bin/componentize-qjs`;
const runtime = process.env.COMPONENTIZE_QJS_RUNTIME ?? `${here}/.cache/toolchain/bin/runtime.wasm`;
await mkdir(`${here}/build`, { recursive: true });
const world = await createWorld([{ path: `${here}/wit`, world: "orb" }], "quickjs");
if (world.wit !== (await readFile(`${here}/assets/engine.wit`, "utf8"))) {
  throw new Error(
    "Generated contract differs from native bindings; update assets/engine.wit deliberately",
  );
}
const validated = validateBootstrap(world.bootstrap, world);
await writeFile(`${here}/build/engine.js`, validated.source);
const command = [
  "--wit",
  "assets/engine.wit",
  "--world",
  "engine",
  "--js",
  "build/engine.js",
  "--output",
  "build/engine.wasm",
  "--stub-wasi",
  "--runtime",
  runtime,
];
const built = spawnSync(generator, command, { cwd: here, encoding: "utf8", timeout: 120000 });
await writeFile(
  `${here}/build/preparation.json`,
  JSON.stringify(
    {
      command: [generator, ...command],
      exit_code: built.status,
      stdout: built.stdout,
      stderr: built.stderr,
    },
    null,
    2,
  ) + "\n",
);
if (built.status !== 0) throw new Error(built.error?.message ?? built.stderr);
const hash = createHash("sha256")
  .update(await readFile(`${here}/build/engine.wasm`))
  .digest("hex");
console.log(`Prepared build/engine.wasm: ${hash}`);
process.exit(0);
