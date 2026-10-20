import { test } from "bun:test";
import assert from "node:assert/strict";
import { mkdtemp, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";

import { build, ToolingError } from "@tembra/sdk/tooling";
import type { ToolingConfig } from "@tembra/sdk/tooling";

async function fixture(run: (root: string, config: ToolingConfig) => Promise<void>): Promise<void> {
  const root = await mkdtemp(path.join(tmpdir(), "tembra-behavioral-contracts-"));
  try {
    await writeFile(path.join(root, "facet.ts"), "export default { value: 1 };\n");
    await run(root, { root, builds: { server: { entry: "facet.ts", platform: "server" } } });
  } finally {
    await rm(root, { recursive: true, force: true });
  }
}

function rejectsExisting(error: unknown): boolean {
  return (
    error instanceof ToolingError && error.phase === "write" && error.reason === "existing_output"
  );
}

test("modified and incomplete publications refuse without repairing caller-owned files", async () => {
  await fixture(async (root, config) => {
    const result = await build(config);
    const [output] = result.artifacts;
    assert.ok(output);
    const envelope = await readFile(output.artifactPath);
    const source = await readFile(output.sourcePath);

    await writeFile(output.sourcePath, "caller-owned modification");
    await assert.rejects(build(config), rejectsExisting);
    assert.equal(await readFile(output.sourcePath, "utf-8"), "caller-owned modification");
    assert.deepEqual(await readFile(output.artifactPath), envelope);

    await writeFile(output.sourcePath, source);
    await rm(output.artifactPath);
    await assert.rejects(build(config), rejectsExisting);
    await assert.rejects(readFile(output.artifactPath), { code: "ENOENT" });
    assert.deepEqual(await readFile(output.sourcePath), source);
    assert.deepEqual(await readdir(path.join(root, "dist/tembra/server")), [
      path.basename(path.dirname(output.sourcePath)),
    ]);

    await writeFile(output.artifactPath, envelope);
    assert.deepEqual(await build(config), result);
  });
});

test("build captures nested caller configuration before its first asynchronous boundary", async () => {
  await fixture(async (root) => {
    await writeFile(
      path.join(root, "facet.ts"),
      "declare const VALUE: number; export default VALUE;",
    );
    const define = { VALUE: "1" };
    const target = { entry: "facet.ts", platform: "server" as const, define };
    const config = { root, builds: { server: target } };
    const admitted = build(config);
    define.VALUE = "2";
    target.entry = "missing.ts";
    const result = await admitted;
    const [output] = result.artifacts;
    assert.ok(output);
    const module: unknown = await import(
      `data:text/javascript;base64,${Buffer.from(output.artifact.source).toString("base64")}`
    );
    assert.ok(module !== null && typeof module === "object" && "default" in module);
    assert.equal(module.default, 1);
    assert.equal(await readFile(output.sourcePath, "utf-8"), output.artifact.source);
  });
});
