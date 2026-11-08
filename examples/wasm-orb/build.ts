// Build the calculator source Orb and, optionally, package a compiled Rust provider.
// Run with Bun: bun build.ts [--provider <component.wasm>] [--out <directory>]
import path from "node:path";
import { parseArgs } from "node:util";
import { build, type BuildTarget } from "@syntropika/tembra-sdk/tooling";

const { values } = parseArgs({
  options: { provider: { type: "string" }, out: { type: "string" } },
});

const builds: Record<string, BuildTarget> = {};

// Compiled Orb: the component carries no Tembra metadata, so the target supplies its identity.
if (values.provider) {
  builds.arithmetic = {
    entry: path.resolve(values.provider),
    wit: "contract.wit",
    world: "arithmetic-provider",
    apiVersion: 1,
  };
}

// Source Orb: generate WIT types, typecheck orb.ts and bundle it with its installed npm closure.
builds.calculator = { entry: "orb.ts", wit: "contract.wit", world: "calculator" };

const artifacts = await build({
  root: import.meta.dir,
  outDir: values.out ? path.resolve(values.out) : "dist/tembra",
  builds,
});
for (const artifact of artifacts) console.log(`${artifact.name}\t${artifact.path}`);
