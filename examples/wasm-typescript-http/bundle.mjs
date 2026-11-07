// Rebuild source bundles without invoking the Wasm component builder.
import { bundleComponentChunk } from "./node_modules/@bytecodealliance/jco/dist/bundle.js";
import { engineCompatPlugin } from "./node_modules/@bytecodealliance/jco/dist/engine-compat.js";
import { readFile, writeFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { createHash } from "node:crypto";
const here = dirname(fileURLToPath(import.meta.url));
const sha = (bytes) => createHash("sha256").update(bytes).digest("hex");
const engineBefore = sha(await readFile(`${here}/assets/engine.wasm`));
const source = await readFile(`${here}/fixtures/analysis-a.ts`, "utf8");
if (source.split("const calibration = 0;").length !== 2)
  throw new Error("Expected one source edit");
await writeFile(
  `${here}/fixtures/analysis-b.ts`,
  source.replace("const calibration = 0;", "const calibration = 1.5;"),
);
const records = [];
for (const version of ["a", "b"]) {
  const entry = `${here}/fixtures/analysis-${version}.ts`;
  const chunk = await bundleComponentChunk(entry, {
    typescript: true,
    aliases: { zod: resolve(here, "node_modules/zod/index.js") },
    external: ["tembra:runtime/context@0.1.0", "example:readings/capabilities"],
    plugins: [engineCompatPlugin()],
  });
  if (
    chunk.imports.some(
      (name) => !["tembra:runtime/context@0.1.0", "example:readings/capabilities"].includes(name),
    )
  )
    throw new Error("Unexpected unresolved import");
  const output = `${here}/build/analysis-${version}.js`;
  await writeFile(output, chunk.code);
  records.push({
    version,
    source: entry,
    source_sha256: sha(await readFile(entry)),
    bundle: output,
    bundle_sha256: sha(chunk.code),
    imports: chunk.imports,
  });
}
const engineAfter = sha(await readFile(`${here}/assets/engine.wasm`));
if (engineBefore !== engineAfter) throw new Error("Source edit changed Wasm engine");
await writeFile(
  `${here}/build/bundles.json`,
  JSON.stringify(
    {
      records,
      engine_sha256_before: engineBefore,
      engine_sha256_after: engineAfter,
      component_builds_during_source_edit: 0,
    },
    null,
    2,
  ) + "\n",
);
console.log("Bundled two TypeScript versions with Zod; unchanged Wasm engine");
process.exit(0);
