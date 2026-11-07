// Generate authoring types from the same contract as the prepared engine.
import { createWorld } from "@bytecodealliance/jco-transpile";
import { guestTypes } from "../node_modules/@bytecodealliance/jco/dist/cmd/types.js";
import { mkdir, readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";

const here = resolve(dirname(fileURLToPath(import.meta.url)), "..");
await mkdir(`${here}/build`, { recursive: true });
const world = await createWorld([{ path: `${here}/wit`, world: "orb" }], "quickjs");
if (world.wit !== (await readFile(`${here}/assets/engine.wit`, "utf8"))) {
  throw new Error("WIT changed: prepare and verify a matching engine before running");
}
await guestTypes(`${here}/wit`, {
  worldName: "orb",
  name: "orb",
  outDir: `${here}/build/types`,
  variantsInlineCases: true,
  quiet: true,
});
console.log("Generated TypeScript declarations from WIT");
process.exit(0);
