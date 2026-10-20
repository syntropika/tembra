import { test } from "bun:test";
import assert from "node:assert/strict";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";

const cli = path.resolve(import.meta.dir, "../bin/tembra.ts");

async function fixture(run: (root: string) => Promise<void>): Promise<void> {
  const root = await mkdtemp(path.join(tmpdir(), "tembra-diagnostic-"));
  try {
    await writeFile(path.join(root, "package.json"), '{"type":"module"}');
    await run(root);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
}

async function failedCli(root: string): Promise<string> {
  const child = Bun.spawn([process.execPath, "run", cli, "build"], {
    cwd: root,
    stdout: "pipe",
    stderr: "pipe",
  });
  const [code, stderr] = await Promise.all([child.exited, new Response(child.stderr).text()]);
  assert.equal(code, 1, stderr);
  return stderr;
}

test("CLI bundle diagnostics locate the importing dependency", async () => {
  await fixture(async (root) => {
    await writeFile(
      path.join(root, "tembra.config.ts"),
      'export default { builds: { server: { entry: "facet.ts", platform: "server" } } };',
    );
    await writeFile(
      path.join(root, "facet.ts"),
      '/// <reference path="./ambient.d.ts" />\nexport { default } from "./dependency.js";',
    );
    await writeFile(
      path.join(root, "ambient.d.ts"),
      'declare module "missing-diagnostic-package" { const value: string; export default value; }',
    );
    await writeFile(
      path.join(root, "dependency.ts"),
      'import value from "missing-diagnostic-package";\nexport default value;',
    );
    const stderr = await failedCli(root);
    assert.match(stderr, /dependency\.ts:1:20:/u);
    assert.match(stderr, /missing-diagnostic-package/u);
    assert.match(stderr, /bundle\/build/u);
  });
});

test("CLI native syntax diagnostics preserve Bun's exact one-based column", async () => {
  await fixture(async (root) => {
    await writeFile(path.join(root, "tembra.config.ts"), "const x = ;\nexport default {};\n");
    const stderr = await failedCli(root);
    assert.match(stderr, /tembra\.config\.ts:1:11: /u);
    assert.match(stderr, /Unexpected/u);
  });
});

test("CLI native missing-export diagnostics preserve Bun's exact one-based column", async () => {
  await fixture(async (root) => {
    await writeFile(
      path.join(root, "tembra.config.ts"),
      'export default { builds: { server: { entry: "facet.ts", platform: "server" } } };',
    );
    await writeFile(
      path.join(root, "facet.ts"),
      'import { missing } from "./dependency.js";\nexport default missing;',
    );
    await writeFile(path.join(root, "dependency.d.ts"), "export const missing: string;");
    await writeFile(path.join(root, "dependency.js"), 'export const actual = "present";');
    const stderr = await failedCli(root);
    assert.match(stderr, /facet\.ts:1:10: /u);
    assert.match(stderr, /No matching export/u);
  });
});

test("CLI retains nested aggregate failures and their original causes", async () => {
  await fixture(async (root) => {
    await writeFile(
      path.join(root, "tembra.config.ts"),
      `throw new AggregateError([
        new Error("first failure", { cause: new Error("original cause") }),
        { message: "foreign failure", cause: { message: "foreign cause" } },
      ], "configuration aggregate");`,
    );
    const stderr = await failedCli(root);
    assert.match(stderr, /configuration aggregate/u);
    assert.match(stderr, /error 1: first failure\ncaused by: original cause/u);
    assert.match(stderr, /error 2: foreign failure\ncaused by: foreign cause/u);
  });
});
