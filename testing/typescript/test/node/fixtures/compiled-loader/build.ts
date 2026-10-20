import { copyFile, mkdir, symlink, writeFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import type { EsmArtifactV1 } from "@tembra/sdk";
import { build } from "@tembra/sdk/tooling";

// Test tooling runs in Bun; the Node Host receives only the resulting artifact envelopes.
const [root] = process.argv.slice(2);
if (root === undefined || root === "") {
  throw new Error("compiled fixture output directory is required");
}
await mkdir(path.join(root, "node_modules/@tembra"), { recursive: true });
await symlink(
  fileURLToPath(new URL("../../../../../../packages/sdk/", import.meta.url)),
  path.join(root, "node_modules/@tembra/sdk"),
  "dir",
);
await copyFile(new URL("facet.mjs", import.meta.url), path.join(root, "facet.mjs"));
await writeFile(path.join(root, "package.json"), '{"type":"module"}');
await writeFile(path.join(root, "bun.lock"), '{"lockfileVersion":1}\n');
await writeFile(
  path.join(root, "tsconfig.json"),
  JSON.stringify({
    compilerOptions: {
      target: "ES2022",
      module: "ESNext",
      moduleResolution: "Bundler",
      allowJs: true,
      checkJs: false,
      skipLibCheck: true,
      types: [],
      lib: ["ES2023", "DOM"],
    },
    include: ["*.mjs"],
  }),
);
const artifacts: { step: number; target: string; artifact: EsmArtifactV1 }[] = [];
for (const step of [1, 2]) {
  // Rebuild a transitive dependency through the actual compiler, not a modified output envelope.
  // oxlint-disable-next-line no-await-in-loop -- Publish each dependency revision before its build.
  await writeFile(path.join(root, "dependency.mjs"), `export const step=${step};\n`);
  // oxlint-disable-next-line no-await-in-loop -- Both revisions use the same source tree.
  const result = await build({
    root,
    builds: {
      plain: { entry: "facet.mjs", platform: "server" },
      minified: { entry: "facet.mjs", platform: "server", minify: true },
    },
  });
  for (const output of result.artifacts) {
    artifacts.push({ step, target: output.target, artifact: output.artifact });
  }
}
await writeFile(path.join(root, "artifacts.json"), JSON.stringify(artifacts));
