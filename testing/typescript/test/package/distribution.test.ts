import { test } from "bun:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

test("published SDK contains current production entrypoints and excludes retired or test outputs", () => {
  const sdk = fileURLToPath(new URL("../../../../packages/sdk/", import.meta.url));
  const result: unknown = JSON.parse(
    execFileSync("npm", ["pack", "--dry-run", "--json", sdk], {
      encoding: "utf-8",
      stdio: ["ignore", "pipe", "pipe"],
    }),
  );
  assert.ok(Array.isArray(result));
  const metadata: unknown = result[0];
  assert.ok(metadata !== null && typeof metadata === "object" && "files" in metadata);
  assert.ok(Array.isArray(metadata.files));
  const files = metadata.files.map((file: unknown) => {
    assert.ok(
      file !== null && typeof file === "object" && "path" in file && typeof file.path === "string",
    );
    return file.path;
  });
  for (const entry of ["index", "node/index", "browser/index", "tooling/index"]) {
    assert.ok(files.includes(`dist/src/${entry}.js`));
    assert.ok(files.includes(`dist/src/${entry}.d.ts`));
  }
  const manifest: unknown = JSON.parse(readFileSync(path.join(sdk, "package.json"), "utf-8"));
  assert.ok(manifest !== null && typeof manifest === "object" && "exports" in manifest);
  assert.equal("bin" in manifest, false);
  assert.ok(manifest.exports !== null && typeof manifest.exports === "object");
  assert.equal("./tooling/cli" in manifest.exports, false);
  assert.equal(
    files.some((file) => file.startsWith("dist/bin/")),
    false,
  );
  assert.equal(
    files.some((file) =>
      /dist\/src\/tooling\/(?:cli|cli-modules|embedded-assets|runtime-assets)\./u.test(file),
    ),
    false,
  );
  assert.equal(
    files.some((file) => /^dist\/(?:test|examples|packages)\//u.test(file)),
    false,
  );
  assert.equal(
    files.some((file) => file.startsWith("dist/src/browser/client.")),
    false,
  );
  assert.equal(files.includes("dist/bin/tembra"), false);
  if (files.includes("dist/wasm/tembra_browser_wasm.d.ts")) {
    const declarations = readFileSync(
      path.join(sdk, "dist/wasm/tembra_browser_wasm.d.ts"),
      "utf-8",
    );
    assert.doesNotMatch(declarations, /export class (?:WasmReference|WasmReadable)\b/u);
    assert.match(declarations, /export class WasmBoundReference\b/u);
  }
}, 30_000);
