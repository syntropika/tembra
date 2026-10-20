import { test } from "bun:test";
import assert from "node:assert/strict";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";

import { build, loadConfig, ToolingError } from "@tembra/sdk/tooling";

async function fixture(run: (root: string) => Promise<void>): Promise<void> {
  const root = await mkdtemp(path.join(tmpdir(), "tembra-diagnostic-"));
  try {
    await writeFile(path.join(root, "package.json"), '{"type":"module"}');
    await run(root);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
}

async function failedTooling(root: string): Promise<ToolingError> {
  try {
    await build(await loadConfig(path.join(root, "tembra.config.ts")));
  } catch (error) {
    assert.ok(error instanceof ToolingError);
    return error;
  }
  throw new Error("expected a structured tooling refusal");
}

test("programmatic bundle diagnostics locate the importing dependency", async () => {
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
    const failure = await failedTooling(root);
    assert.ok(failure.cause instanceof AggregateError);
    const diagnostic = failure.cause.message;
    assert.match(diagnostic, /dependency\.ts:1:20:/u);
    assert.match(diagnostic, /missing-diagnostic-package/u);
    assert.equal(failure.phase, "bundle");
    assert.equal(failure.reason, "build");
    assert.equal(failure.target, "server");
  });
});

test("programmatic native syntax diagnostics preserve Bun's exact one-based column", async () => {
  await fixture(async (root) => {
    await writeFile(path.join(root, "tembra.config.ts"), "const x = ;\nexport default {};\n");
    const failure = await failedTooling(root);
    assert.ok(failure.cause instanceof AggregateError);
    const diagnostic = failure.cause.message;
    assert.match(diagnostic, /tembra\.config\.ts:1:11: /u);
    assert.match(diagnostic, /Unexpected/u);
  });
});

test("programmatic native missing-export diagnostics preserve Bun's exact one-based column", async () => {
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
    const failure = await failedTooling(root);
    assert.ok(failure.cause instanceof AggregateError);
    const diagnostic = failure.cause.message;
    assert.match(diagnostic, /facet\.ts:1:10: /u);
    assert.match(diagnostic, /No matching export/u);
  });
});

test("programmatic configuration retains nested aggregate failures and their original causes", async () => {
  await fixture(async (root) => {
    await writeFile(
      path.join(root, "tembra.config.ts"),
      `throw new AggregateError([
        new Error("first failure", { cause: new Error("original cause") }),
        { message: "foreign failure", cause: { message: "foreign cause" } },
      ], "configuration aggregate");`,
    );
    const failure = await failedTooling(root);
    assert.ok(failure.cause instanceof AggregateError);
    const diagnostic = failure.cause.message;
    assert.equal(failure.phase, "config");
    assert.equal(failure.reason, "load_failed");
    assert.match(diagnostic, /configuration aggregate/u);
    const first: unknown = failure.cause.errors[0];
    const second: unknown = failure.cause.errors[1];
    assert.ok(first instanceof Error);
    assert.equal(first.message, "first failure");
    assert.ok(first.cause instanceof Error);
    assert.equal(first.cause.message, "original cause");
    assert.deepEqual(second, { message: "foreign failure", cause: { message: "foreign cause" } });
  });
});
