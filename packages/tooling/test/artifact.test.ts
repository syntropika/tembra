import { test } from "bun:test";
import assert from "node:assert/strict";

import { createArtifact, inspectPlatform } from "../src/artifact.js";
import { ToolingError } from "../src/error.js";

function reason(expected: string): (error: unknown) => boolean {
  return (error) => error instanceof ToolingError && error.reason === expected;
}
test("closed artifacts reject all imports and unsupported export metadata", async () => {
  await Promise.all(
    [
      'import value from "pkg"; export default value;',
      'export default () => import("pkg");',
      "export default import.meta.url;",
    ].map(async (source) => {
      await assert.rejects(createArtifact(source, "facet", "default", "server"), reason("imports"));
    }),
  );
  await assert.rejects(
    createArtifact("export const then = 1;", "facet", "then", "server"),
    reason("exports"),
  );
  await assert.rejects(
    createArtifact("export default 1;", "facet", "missing", "server"),
    reason("exports"),
  );
  await assert.rejects(
    createArtifact('export default "long";', "facet", "default", "server", 5),
    reason("source_size"),
  );
  await assert.rejects(
    createArtifact("export default 1;", "bad\nidentity", "default", "server"),
    reason("metadata"),
  );
});

test("platform diagnostics distinguish actual references from local bindings and property names", () => {
  for (const source of [
    "export default process.version;",
    "export default globalThis.process;",
    'export default globalThis["Buffer"];',
    "const { process: p } = globalThis; export default p;",
    "export default typeof process.version;",
  ]) {
    assert.throws(() => {
      inspectPlatform(source, "browser");
    }, reason("platform_global"));
  }
  for (const source of [
    "const process = {version: 1}; export default process.version;",
    "export default function(process) { return process.version; }",
    "export default { process: 1, TextEncoder: 2 };",
    "const object = {process: 1}; export default object.process;",
    "export default { process: typeof process, window: typeof window };",
  ]) {
    inspectPlatform(source, "server");
  }
  assert.throws(() => {
    inspectPlatform('export default () => fetch("/");', "server");
  }, reason("platform_global"));
  inspectPlatform('export default () => fetch("/");', "browser");
});
