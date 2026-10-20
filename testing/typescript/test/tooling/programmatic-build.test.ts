import { test } from "bun:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdir, mkdtemp, readFile, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { build, loadConfig, ToolingError } from "@tembra/sdk/tooling";

test("programmatic build combines generated contracts, platform conditions, named exports and deterministic sourcemaps", async () => {
  const root = await mkdtemp(path.join(tmpdir(), "tembra-programmatic-build-"));
  try {
    const sdk = path.resolve(
      path.dirname(fileURLToPath(import.meta.resolve("@tembra/sdk"))),
      "../..",
    );
    await mkdir(path.join(root, "node_modules/@tembra"), { recursive: true });
    await symlink(sdk, path.join(root, "node_modules/@tembra/sdk"), "dir");
    const config = path.join(root, "tembra.config.ts");
    await writeFile(
      config,
      'import {defineConfig} from "@tembra/sdk/tooling"; export default defineConfig({ builds: { main: { entry: "main.ts", platform: "server", export: "facet", sourcemap: true }, browser: { entry: "browser.ts", platform: "browser", export: "facet", sourcemap: true } }, codegen: [{ source: { module: "contract.ts", export: "Contract" }, typescript: "generated/counter.ts", name: "Counter" }], outDir: "output" });',
    );
    const dependency = path.join(root, "node_modules/fixture-package");
    await mkdir(dependency, { recursive: true });
    await writeFile(
      path.join(dependency, "package.json"),
      JSON.stringify({
        name: "fixture-package",
        type: "module",
        browser: { fs: false, net: false },
        exports: {
          types: "./index.d.ts",
          node: "./node.cjs",
          bun: "./bun.js",
          browser: "./browser.js",
          default: "./pure.cjs",
        },
      }),
    );
    await writeFile(path.join(dependency, "index.d.ts"), "export const value: string;");
    await writeFile(
      path.join(dependency, "node.cjs"),
      'module.exports = { value: "wrong-node-branch" };',
    );
    await writeFile(path.join(dependency, "bun.js"), 'export const value = "wrong-bun-branch";');
    await writeFile(
      path.join(dependency, "browser.js"),
      'import fs from "fs"; import net from "net"; fs.marker = true; export const value = Object.keys(net).length === 0 ? "browser-package" : "shared-stub";',
    );
    await writeFile(path.join(dependency, "pure.cjs"), 'module.exports = { value: "fixture" };');
    const mainSource =
      'import { value } from "fixture-package"; import { defineFacet, type InputOf } from "@tembra/sdk"; import { Counter } from "./generated/counter.js"; const input: InputOf<typeof Counter> = {name: value}; export const facet = defineFacet({ name: input.name, uses: [Counter], setup() {} });';
    await writeFile(path.join(root, "main.ts"), mainSource);
    await writeFile(
      path.join(root, "browser.ts"),
      'import { value } from "fixture-package"; import { defineFacet } from "@tembra/sdk"; import type { CreateBrowserHostOptions } from "@tembra/sdk/browser"; import { Counter } from "./generated/counter.js"; const options: CreateBrowserHostOptions = {name: value}; export const facet = defineFacet({name: options.name ?? value, uses: [Counter], setup() {}});',
    );
    await writeFile(
      path.join(root, "tsconfig.json"),
      JSON.stringify({
        compilerOptions: { target: "ES2022", module: "ESNext", strict: true },
        include: ["main.ts"],
      }),
    );
    await writeFile(
      path.join(root, "contract.ts"),
      'import { local, Type, method } from "@tembra/sdk"; export const Contract = local("fixture.counter", { version: 1, input: Type.Object({name: Type.String()}), members: { read: method(undefined, Type.Integer()) } });',
    );
    const loaded = await loadConfig(config);
    const first = await build(loaded);
    const outputs = await Promise.all(
      first.artifacts.map(async (output) => ({
        platform: output.platform,
        source: output.artifact.source,
        hash: output.artifact.sha256,
        artifact: await readFile(output.artifactPath, "utf-8"),
        map: await readFile(path.join(path.dirname(output.artifactPath), "facet.js.map"), "utf-8"),
      })),
    );
    assert.equal(outputs.length, 2);
    for (const output of outputs) {
      assert.equal(createHash("sha256").update(output.source).digest("hex"), output.hash);
      const envelope: unknown = JSON.parse(output.artifact);
      assert.ok(envelope !== null && typeof envelope === "object" && "format" in envelope);
      assert.equal(envelope.format, "tembra-esm-v1");
      const map: unknown = JSON.parse(output.map);
      assert.ok(map !== null && typeof map === "object" && "sourcesContent" in map);
      assert.ok(Array.isArray(map.sourcesContent) && map.sourcesContent.length > 0);
      assert.doesNotMatch(output.source, /wrong-(?:node|bun)-branch/u);
    }
    const browser = outputs.find((output) => output.platform === "browser");
    assert.ok(browser);
    const namespace: unknown = await import(
      `data:text/javascript;base64,${Buffer.from(browser.source).toString("base64")}`
    );
    assert.ok(namespace !== null && typeof namespace === "object" && "facet" in namespace);
    const definition: unknown = namespace.facet;
    assert.ok(definition !== null && typeof definition === "object" && "name" in definition);
    assert.equal(definition.name, "browser-package");
    assert.match(
      await readFile(path.join(root, "generated/counter.ts"), "utf-8"),
      /CounterReference/u,
    );
    assert.deepEqual(await build(loaded), first);
    await writeFile(
      path.join(root, "main.ts"),
      `${mainSource} const wrong: number = "bad"; void wrong;`,
    );
    await assert.rejects(
      build(loaded),
      (error) => error instanceof ToolingError && error.phase === "typecheck",
    );
    await Promise.all(
      first.artifacts.map(async (output) => {
        assert.equal(
          await readFile(output.artifactPath, "utf-8"),
          outputs.find((item) => item.platform === output.platform)?.artifact,
        );
      }),
    );
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});
