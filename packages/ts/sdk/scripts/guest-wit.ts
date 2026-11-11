/**
 * Generate the official jco declarations of the private `tembra:orb` package for the guest shim.
 *
 * The output is build output under `src/guest/.tembra/` (ignored), never a hand copy: the canonical
 * WIT is `crates/contracts/wit/tembra/orb.wit`, bundled by the executor.
 */
import { mkdir, readFile, rm, writeFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../", import.meta.url));
const wit = path.resolve(root, "../../../crates/contracts/wit/tembra/orb.wit");
const destination = path.join(root, "src/guest/.tembra");
const { $init, generateTypes } = await import("@bytecodealliance/jco-transpile/component");
await $init;
const generated = generateTypes("source", {
  wit: { tag: "source", val: await readFile(wit, "utf8") },
  world: "source",
});
await rm(destination, { recursive: true, force: true });
for (const [name, bytes] of generated) {
  if (!name.startsWith("interfaces/")) continue;
  const file = path.join(destination, path.basename(name));
  await mkdir(path.dirname(file), { recursive: true });
  await writeFile(file, bytes);
}
