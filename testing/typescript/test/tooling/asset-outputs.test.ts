import { test } from "bun:test";
import assert from "node:assert/strict";
import { mkdtemp, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";

import { build, ToolingError } from "@tembra/sdk/tooling";
import type { Platform, ToolingConfig } from "@tembra/sdk/tooling";

const assets = [
  {
    extension: "png",
    bytes: Buffer.from(
      "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jPioAAAAASUVORK5CYII=",
      "base64",
    ),
  },
  { extension: "wasm", bytes: new Uint8Array([0, 97, 115, 109, 1, 0, 0, 0]) },
  { extension: "css", bytes: Buffer.from("body { color: red; }", "utf-8") },
];
const platforms: readonly Platform[] = ["server", "browser"];

test.each(assets.flatMap((asset) => platforms.map((platform) => ({ ...asset, platform }))))(
  "asset imports refuse publication on every executor",
  async ({ extension, bytes, platform }) => {
    const root = await mkdtemp(path.join(tmpdir(), "tembra-asset-output-"));
    try {
      await writeFile(path.join(root, "package.json"), '{"type":"module"}');
      await writeFile(path.join(root, "safe.ts"), 'export default "previous safe";');
      await writeFile(path.join(root, "asset.ts"), 'export default "previous asset";');
      const config: ToolingConfig = {
        root,
        builds: {
          safe: { entry: "safe.ts", platform },
          asset: { entry: "asset.ts", platform },
        },
      };
      const previous = await build(config);
      const files = previous.artifacts.flatMap((artifact) => [
        artifact.sourcePath,
        artifact.artifactPath,
      ]);
      const contents = await Promise.all(files.map(async (file) => await readFile(file, "utf-8")));
      const outDir = path.join(root, "dist/tembra");
      const published = await readdir(outDir, { recursive: true });
      await writeFile(path.join(root, "safe.ts"), 'export default "candidate safe";');
      await writeFile(path.join(root, `payload.${extension}`), bytes);
      await writeFile(
        path.join(root, "ambient.d.ts"),
        `declare module "*.${extension}" { const url: string; export default url; }`,
      );
      const importSource =
        extension === "css"
          ? 'import "./payload.css"; export default "styles";'
          : `import url from "./payload.${extension}"; export default url;`;
      await writeFile(
        path.join(root, "asset.ts"),
        `/// <reference path="./ambient.d.ts" />\n${importSource}`,
      );
      await assert.rejects(
        build(config),
        (error: unknown) =>
          error instanceof ToolingError &&
          error.phase === "bundle" &&
          error.reason === "output" &&
          error.target === "asset",
      );
      const retained = await readdir(outDir, { recursive: true });
      assert.deepEqual(retained.toSorted(), published.toSorted());
      assert.deepEqual(
        await Promise.all(files.map(async (file) => await readFile(file, "utf-8"))),
        contents,
      );
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  },
);
