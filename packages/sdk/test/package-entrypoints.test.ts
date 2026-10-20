import { test } from "bun:test";
import assert from "node:assert/strict";
import path from "node:path";
import { fileURLToPath } from "node:url";

test("public SDK subpaths share a package while the root graph excludes browser execution", async () => {
  const rootEntry = import.meta.resolve("@tembra/sdk");
  const browserEntry = import.meta.resolve("@tembra/sdk/browser");
  assert.notEqual(rootEntry, browserEntry);
  const root: unknown = await import(rootEntry);
  const browser: unknown = await import(browserEntry);
  assert.ok(root !== null && typeof root === "object" && "defineFacet" in root);
  assert.equal(typeof root.defineFacet, "function");
  assert.equal("BrowserClient" in root, false);
  assert.ok(browser !== null && typeof browser === "object" && "BrowserClient" in browser);
  assert.equal(typeof browser.BrowserClient, "function");

  const inputs: string[] = [];
  const result = await Bun.build({
    entrypoints: [fileURLToPath(rootEntry)],
    target: "browser",
    format: "esm",
    plugins: [
      {
        name: "record-root-import-graph",
        setup(builder) {
          builder.onLoad({ filter: /.*/u }, (args) => {
            inputs.push(args.path.split(path.sep).join("/"));
            return undefined;
          });
        },
      },
    ],
  });
  assert.equal(result.success, true, result.logs.map(String).join("\n"));
  assert.ok(inputs.length > 0);
  assert.equal(
    inputs.some((input) => input.includes("/src/browser/")),
    false,
  );
  assert.equal(
    inputs.some((input) => input.includes("es-module-lexer")),
    false,
  );
  assert.equal(
    inputs.some((input) => input.includes("/tooling/")),
    false,
  );
  assert.equal(
    inputs.some((input) => input.includes("/typescript/")),
    false,
  );
});
