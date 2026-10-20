import { test } from "bun:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";

async function importGraph(request: string, target: "node" | "browser"): Promise<string[]> {
  const child = Bun.spawn(
    [
      process.execPath,
      fileURLToPath(new URL("entrypoint-graph.ts", import.meta.url)),
      request,
      target,
    ],
    { stdout: "pipe", stderr: "pipe" },
  );
  const [exitCode, stdout, stderr] = await Promise.all([
    child.exited,
    new Response(child.stdout).text(),
    new Response(child.stderr).text(),
  ]);
  assert.equal(exitCode, 0, stderr);
  const inputs: unknown = JSON.parse(stdout);
  assert.ok(Array.isArray(inputs) && inputs.every((input) => typeof input === "string"));
  assert.equal(
    inputs.some((input) => /(?:\/testing\/|\/packages\/sdk\/(?:test|examples)\/)/u.test(input)),
    false,
    "production SDK entrypoints must not import test frameworks, fixtures or examples",
  );
  return inputs;
}

test("public SDK subpaths share a package while the root graph excludes browser execution", async () => {
  const rootEntry = import.meta.resolve("@tembra/sdk");
  const browserEntry = import.meta.resolve("@tembra/sdk/browser");
  assert.notEqual(rootEntry, browserEntry);
  const root: unknown = await import(rootEntry);
  const browser: unknown = await import(browserEntry);
  assert.ok(root !== null && typeof root === "object" && "defineFacet" in root);
  assert.equal(typeof root.defineFacet, "function");
  assert.equal("BrowserClient" in root, false);
  assert.ok(browser !== null && typeof browser === "object" && "createHost" in browser);
  assert.equal(typeof browser.createHost, "function");
  for (const internal of ["BrowserClient", "BrowserExecutor", "WasmBindingPort"]) {
    assert.equal(internal in browser, false);
  }

  const inputs = await importGraph("@tembra/sdk", "browser");
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
    inputs.some((input) => input.includes("/typescript/") || input.includes("/jiti/")),
    false,
  );
});

test("platform Host entrypoints exclude the opposite execution graph", async () => {
  for (const platform of ["node", "browser"] as const) {
    const entry = import.meta.resolve(`@tembra/sdk/${platform}`);
    // oxlint-disable-next-line no-await-in-loop -- Each platform graph is inspected independently and serially.
    const module: unknown = await import(entry);
    assert.ok(module !== null && typeof module === "object" && "createHost" in module);
    assert.equal(typeof module.createHost, "function");
    // oxlint-disable-next-line no-await-in-loop -- Each compiler gets an independent process and graph report.
    const inputs = await importGraph(`@tembra/sdk/${platform}`, platform);
    assert.equal(
      inputs.some((input) => input.includes(`/src/${platform === "node" ? "browser" : "node"}/`)),
      false,
    );
    assert.equal(
      inputs.some((input) => input.endsWith(".node") || input.endsWith(".wasm")),
      false,
    );
    assert.equal(
      inputs.some((input) => input.includes("/tooling/") || input.includes("/typescript/")),
      false,
    );
    if (platform === "browser") {
      assert.equal(
        inputs.some((input) => input.includes("/jiti/")),
        false,
      );
    }
    if (platform === "node") {
      assert.equal(
        inputs.some((input) => input.includes("es-module-lexer")),
        true,
      );
      assert.equal(
        inputs.some((input) => /\/src\/host\/artifact\.[jt]s$/u.test(input)),
        true,
      );
    }
  }
});

test("tooling subpaths expose build-time APIs and an isolated browser development client", async () => {
  const tooling = await import("@tembra/sdk/tooling");
  const browserTooling = await import("@tembra/sdk/tooling/browser");
  for (const operation of ["build", "dev", "defineConfig", "generateBindings"] as const) {
    assert.equal(typeof tooling[operation], "function");
  }
  assert.equal(typeof browserTooling.connectDevelopmentHost, "function");
  assert.equal("runCli" in tooling, false);
  const inputs = await importGraph("@tembra/sdk/tooling/browser", "browser");
  assert.equal(
    inputs.some(
      (input) =>
        input.includes("/typescript/") || input.includes("/jiti/") || input.includes("/src/node/"),
    ),
    false,
  );
});
