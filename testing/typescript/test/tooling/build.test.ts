import { test } from "bun:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdtemp, mkdir, readFile, readdir, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

import { build, ToolingError } from "@tembra/sdk/tooling";
import type { ToolingConfig } from "@tembra/sdk/tooling";

import { canonicalSdk } from "../../../../packages/sdk/src/tooling/bundler.js";

async function fixture(run: (root: string, config: ToolingConfig) => Promise<void>): Promise<void> {
  const root = await mkdtemp(path.join(tmpdir(), "tembra-build-"));
  try {
    await writeFile(path.join(root, "package.json"), '{"type":"module"}');
    await writeFile(path.join(root, "bun.lock"), '{"lockfileVersion":1}\n');
    await writeFile(path.join(root, "facet.ts"), "export default { value: 1 };\n");
    await run(root, { root, builds: { server: { entry: "facet.ts", platform: "server" } } });
  } finally {
    await rm(root, { recursive: true, force: true });
  }
}
function phase(expected: string): (error: unknown) => boolean {
  return (error) => error instanceof ToolingError && error.phase === expected;
}

test("production envelope uses final bytes and publishes deterministic source and maps", async () => {
  await fixture(async (root, config) => {
    const mapped: ToolingConfig = {
      ...config,
      builds: { server: { ...required(config.builds.server), sourcemap: true } },
    };
    const first = await build(mapped);
    const second = await build(mapped);
    assert.deepEqual(first, second);
    const output = required(first.artifacts[0]);
    assert.deepEqual(Object.keys(output.artifact).toSorted(), [
      "exports",
      "format",
      "identity",
      "sha256",
      "source",
    ]);
    assert.equal(
      output.artifact.sha256,
      createHash("sha256").update(output.artifact.source).digest("hex"),
    );
    assert.equal(await readFile(output.sourcePath, "utf-8"), output.artifact.source);
    assert.deepEqual(JSON.parse(await readFile(output.artifactPath, "utf-8")), output.artifact);
    const mapPath = required(output.sourcemapPath);
    const map: unknown = JSON.parse(await readFile(mapPath, "utf-8"));
    assert.ok(map !== null && typeof map === "object" && "sources" in map);
    assert.ok(Array.isArray(map.sources));
    const sourceRoot = "sourceRoot" in map ? map.sourceRoot : "";
    assert.equal(typeof sourceRoot, "string");
    assert.ok(typeof sourceRoot === "string");
    const base = new URL(sourceRoot, pathToFileURL(mapPath));
    const originalSources = map.sources.map((source: unknown) => {
      assert.ok(typeof source === "string");
      return fileURLToPath(new URL(source, base));
    });
    assert.ok(originalSources.includes(path.join(root, "facet.ts")));
    assert.ok(first.inputs.includes(path.join(root, "bun.lock")));
    assert.ok(first.inputs.includes(path.join(root, "facet.ts")));
    const published = await readdir(path.join(root, "dist/tembra/server"));
    assert.equal(published.length, 1);
  });
});

test("absolute entry paths retain literal fragment and query characters", async () => {
  await fixture(async (root) => {
    const project = path.join(root, process.platform === "win32" ? "project#1" : "project#1?query");
    await mkdir(project);
    await writeFile(path.join(project, "dependency.ts"), 'export const value = "literal-path";');
    await writeFile(
      path.join(project, "facet.ts"),
      'import {value} from "./dependency.js"; export default {value};',
    );
    const config: ToolingConfig = {
      root: project,
      builds: {
        server: { entry: "facet.ts", platform: "server", sourcemap: true },
        browser: { entry: "facet.ts", platform: "browser", sourcemap: true },
      },
    };
    const result = await build(config);
    assert.equal(result.artifacts.length, 2);
    assert.ok(result.inputs.includes(path.join(project, "facet.ts")));
    assert.ok(result.inputs.includes(path.join(project, "dependency.ts")));
    assert.deepEqual(await build(config), result);
    const modules: unknown[] = await Promise.all(
      result.artifacts.map(async (output) => {
        const namespace: unknown = await import(
          `data:text/javascript;base64,${Buffer.from(output.artifact.source).toString("base64")}`
        );
        return namespace;
      }),
    );
    for (const namespace of modules) {
      assertRecord(namespace);
      assert.deepEqual(namespace.default, { value: "literal-path" });
    }
  });
});

test("legacy node resolution does not receive unsupported custom conditions", async () => {
  await fixture(async (root, config) => {
    await writeFile(
      path.join(root, "tsconfig.json"),
      JSON.stringify({ compilerOptions: { module: "esnext", moduleResolution: "node" } }),
    );
    const result = await build(config);
    assert.equal(result.artifacts.length, 1);
  });
});

test("compatible npm uses platform export conditions and transitive inputs", async () => {
  await fixture(async (root, config) => {
    const directory = path.join(root, "node_modules/compatible");
    await mkdir(directory, { recursive: true });
    await writeFile(
      path.join(directory, "package.json"),
      JSON.stringify({
        name: "compatible",
        type: "module",
        exports: {
          types: "./index.d.ts",
          node: "./node.js",
          bun: "./bun.js",
          browser: "./browser.js",
          custom: "./custom.js",
          default: "./server.js",
        },
      }),
    );
    await writeFile(path.join(directory, "index.d.ts"), "export const value: string;");
    await writeFile(path.join(directory, "server.js"), 'export const value = "server";');
    await Promise.all([
      writeFile(path.join(directory, "browser.js"), 'export const value = "browser";'),
      writeFile(path.join(directory, "node.js"), 'export const value = "node";'),
      writeFile(path.join(directory, "bun.js"), 'export const value = "bun";'),
      writeFile(path.join(directory, "custom.js"), 'export const value = "custom";'),
    ]);
    const commonjs = path.join(root, "node_modules/pure-commonjs");
    const legacy = path.join(root, "node_modules/legacy-module");
    await Promise.all([mkdir(commonjs), mkdir(legacy)]);
    await Promise.all([
      writeFile(
        path.join(commonjs, "package.json"),
        JSON.stringify({ main: "index.cjs", types: "index.d.ts" }),
      ),
      writeFile(path.join(commonjs, "index.cjs"), 'module.exports = { value: "commonjs" };'),
      writeFile(
        path.join(commonjs, "index.d.ts"),
        "declare const value: { value: string }; export = value;",
      ),
      writeFile(
        path.join(legacy, "package.json"),
        JSON.stringify({
          main: "main.cjs",
          module: "module.js",
          types: "index.d.ts",
          browser: { "./module.js": "./browser.js" },
        }),
      ),
      writeFile(path.join(legacy, "main.cjs"), 'module.exports = { legacy: "legacy-main" };'),
      writeFile(path.join(legacy, "module.js"), 'export const legacy = "legacy-module";'),
      writeFile(path.join(legacy, "browser.js"), 'export const legacy = "legacy-browser";'),
      writeFile(path.join(legacy, "index.d.ts"), "export const legacy: string;"),
      writeFile(path.join(root, "aliased.ts"), 'export const alias = "alias";'),
      writeFile(
        path.join(root, "tsconfig.json"),
        JSON.stringify({
          compilerOptions: {
            paths: { "@fixture/value": ["./aliased.ts"] },
          },
        }),
      ),
    ]);
    await writeFile(
      path.join(root, "facet.ts"),
      'import { value } from "compatible"; import commonjs from "pure-commonjs"; import { legacy } from "legacy-module"; import { alias } from "@fixture/value"; export default { value, commonjs, legacy, alias };',
    );
    const result = await build({
      ...config,
      builds: {
        server: required(config.builds.server),
        browser: { entry: "facet.ts", platform: "browser", minify: true },
        custom: { entry: "facet.ts", platform: "server", conditions: ["custom"] },
      },
    });
    const server = required(result.artifacts.find((artifact) => artifact.target === "server"));
    const browser = required(result.artifacts.find((artifact) => artifact.platform === "browser"));
    assert.match(server.artifact.source, /"server"/u);
    assert.match(browser.artifact.source, /"browser"/u);
    assert.ok(server.inputs.includes(path.join(directory, "server.js")));
    assert.ok(!server.inputs.includes(path.join(directory, "browser.js")));
    assert.ok(!server.inputs.includes(path.join(directory, "node.js")));
    assert.ok(!server.inputs.includes(path.join(directory, "bun.js")));
    assert.ok(!server.inputs.includes(path.join(directory, "custom.js")));
    assert.ok(server.inputs.includes(path.join(root, "aliased.ts")));
    assert.ok(server.inputs.includes(path.join(legacy, "module.js")));
    assert.ok(!server.inputs.includes(path.join(legacy, "browser.js")));
    assert.ok(!server.inputs.includes(path.join(legacy, "main.cjs")));
    const custom = required(result.artifacts.find((artifact) => artifact.target === "custom"));
    assert.ok(custom.inputs.includes(path.join(directory, "custom.js")));
    assert.ok(browser.inputs.includes(path.join(legacy, "browser.js")));
    const namespace: unknown = await import(
      `data:text/javascript;base64,${Buffer.from(server.artifact.source).toString("base64")}`
    );
    assert.ok(namespace !== null && typeof namespace === "object" && "default" in namespace);
    assert.deepEqual(namespace.default, {
      value: "server",
      commonjs: { value: "commonjs" },
      legacy: "legacy-module",
      alias: "alias",
    });
  });
});

test("semantic type errors retain previous published output and never partially publish targets", async () => {
  await fixture(async (root, config) => {
    const initial = await build(config);
    const source = await readFile(required(initial.artifacts[0]).artifactPath, "utf-8");
    await writeFile(path.join(root, "facet.ts"), "export default { value: 2 };");
    await writeFile(
      path.join(root, "broken.ts"),
      'const value: number = "wrong"; export default { value };',
    );
    await assert.rejects(
      build({
        ...config,
        builds: {
          first: required(config.builds.server),
          second: { entry: "broken.ts", platform: "server" },
        },
      }),
      phase("typecheck"),
    );
    assert.deepEqual(await readdir(path.join(root, "dist/tembra")), ["server"]);
    assert.equal(await readFile(required(initial.artifacts[0]).artifactPath, "utf-8"), source);
  });
});

test("selected target and export filtering leave unrelated invalid entries alone", async () => {
  await fixture(async (root, config) => {
    await writeFile(path.join(root, "facet.ts"), "export const selected = { value: 1 };");
    const filtered = {
      ...config,
      builds: {
        good: { ...required(config.builds.server), export: "selected" },
        bad: { entry: "missing.ts", platform: "server" as const },
      },
    };
    const result = await build(filtered, { targets: ["good"] });
    assert.deepEqual(
      result.artifacts.map((artifact) => artifact.target),
      ["good"],
    );
    assert.deepEqual(required(result.artifacts[0]).artifact.exports, ["selected"]);
    await assert.rejects(build({ ...config }), phase("artifact"));
    await assert.rejects(build(filtered, { targets: ["unknown"] }), phase("config"));
  });
});

test("Node builtins cannot escape into either executor", async () => {
  await fixture(async (root, config) => {
    await writeFile(
      path.join(root, "facet.ts"),
      '/// <reference path="./ambient.d.ts" />\nimport * as fs from "node:fs"; export default fs;',
    );
    await writeFile(
      path.join(root, "ambient.d.ts"),
      'declare module "node:fs" { const value: unknown; export = value; }',
    );

    await assert.rejects(build(config), phase("bundle"));
    await assert.rejects(
      build({ ...config, builds: { browser: { entry: "facet.ts", platform: "browser" } } }),
      phase("bundle"),
    );
  });
});

test("browser remaps and installed builtin-named packages resolve before host refusal", async () => {
  await fixture(async (root, config) => {
    const remapped = path.join(root, "node_modules/remapped-host");
    const events = path.join(root, "node_modules/events");
    const ws = path.join(root, "node_modules/ws");
    await Promise.all([
      mkdir(remapped, { recursive: true }),
      mkdir(events, { recursive: true }),
      mkdir(ws, { recursive: true }),
    ]);
    await Promise.all([
      writeFile(
        path.join(remapped, "package.json"),
        JSON.stringify({
          name: "remapped-host",
          type: "module",
          main: "index.js",
          types: "index.d.ts",
          browser: { util: "./util.js", fs: false, net: false },
        }),
      ),
      writeFile(path.join(remapped, "index.d.ts"), "export const value: string;"),
      writeFile(
        path.join(remapped, "index.js"),
        'import { inspect } from "util"; import fs from "fs"; import net from "net"; fs.marker = true; export const value = inspect("mapped") + Object.keys(net).length;',
      ),
      writeFile(path.join(remapped, "util.js"), 'export const inspect = (value) => value + "-";'),
      writeFile(
        path.join(events, "package.json"),
        JSON.stringify({ name: "events", type: "module", main: "index.js", types: "index.d.ts" }),
      ),
      writeFile(path.join(events, "index.d.ts"), "export const event: string;"),
      writeFile(path.join(events, "index.js"), 'export const event = "installed-events";'),
      writeFile(
        path.join(ws, "package.json"),
        JSON.stringify({ name: "ws", type: "module", main: "index.js", types: "index.d.ts" }),
      ),
      writeFile(path.join(ws, "index.d.ts"), "export const socket: string;"),
      writeFile(path.join(ws, "index.js"), 'export const socket = "installed-ws";'),
      writeFile(
        path.join(root, "facet.ts"),
        'import {event} from "events"; import {socket} from "ws"; export default {event, socket};',
      ),
      writeFile(
        path.join(root, "browser.ts"),
        'import {event} from "events"; import {socket} from "ws"; import {value} from "remapped-host"; export default {event, socket, value};',
      ),
    ]);
    const result = await build({
      ...config,
      builds: {
        server: required(config.builds.server),
        browser: { entry: "browser.ts", platform: "browser" },
      },
    });
    const server = required(result.artifacts.find((output) => output.target === "server"));
    const browser = required(result.artifacts.find((output) => output.target === "browser"));
    const serverModule: unknown = await import(
      `data:text/javascript;base64,${Buffer.from(server.artifact.source).toString("base64")}`
    );
    const browserModule: unknown = await import(
      `data:text/javascript;base64,${Buffer.from(browser.artifact.source).toString("base64")}`
    );
    assertRecord(serverModule);
    assertRecord(browserModule);
    assert.deepEqual(serverModule.default, { event: "installed-events", socket: "installed-ws" });
    assert.deepEqual(browserModule.default, {
      event: "installed-events",
      socket: "installed-ws",
      value: "mapped-0",
    });
    assert.ok(browser.inputs.includes(path.join(remapped, "util.js")));
    assert.ok(browser.inputs.every((file) => path.isAbsolute(file)));
    assert.ok(!browser.inputs.some((file) => file.includes("tembra-empty")));
    await writeFile(
      path.join(root, "facet.ts"),
      '/// <reference path="./ambient.d.ts" />\nimport fs from "fs"; export default fs;',
    );
    await writeFile(
      path.join(root, "ambient.d.ts"),
      'declare module "fs" { const value: unknown; export default value; }',
    );
    await assert.rejects(build(config), phase("bundle"));
    await assert.rejects(
      build({ ...config, builds: { browser: { entry: "facet.ts", platform: "browser" } } }),
      phase("bundle"),
    );
  });
});

test("transitive JavaScript platform globals receive diagnostics", async () => {
  await fixture(async (root, config) => {
    await writeFile(path.join(root, "dependency.js"), "export const value = process.version;");
    await writeFile(path.join(root, "dependency.d.ts"), "export const value: string;");
    await writeFile(
      path.join(root, "facet.ts"),
      'import { value } from "./dependency.js"; export default { value };',
    );
    await assert.rejects(
      build(config),
      (error: unknown) => error instanceof ToolingError && error.reason === "platform_global",
    );
    await writeFile(path.join(root, "dependency.js"), "export const value = process.env.NODE_ENV;");
    await assert.rejects(
      build(config),
      (error: unknown) => error instanceof ToolingError && error.reason === "platform_global",
    );
    await writeFile(path.join(root, "dependency.js"), 'export const value = fetch("/");');
    await assert.rejects(
      build(config),
      (error: unknown) => error instanceof ToolingError && error.reason === "platform_global",
    );
    await build({ ...config, builds: { browser: { entry: "facet.ts", platform: "browser" } } });
  });
});

test("bundles real SDK declarations with a lexical encoder and deduplicates nested SDK copies", async () => {
  await fixture(async (root, config) => {
    const sdk = path.resolve(
      path.dirname(fileURLToPath(import.meta.resolve("@tembra/sdk"))),
      "../..",
    );
    await mkdir(path.join(root, "node_modules/@tembra"), { recursive: true });
    await symlink(sdk, path.join(root, "node_modules/@tembra/sdk"), "dir");
    const dependency = path.join(root, "node_modules/indirect");
    await mkdir(path.join(dependency, "node_modules/@tembra/sdk"), { recursive: true });
    await writeFile(
      path.join(dependency, "package.json"),
      JSON.stringify({
        name: "indirect",
        type: "module",
        exports: { types: "./index.d.ts", default: "./index.js" },
      }),
    );
    await writeFile(path.join(dependency, "index.d.ts"), "export const indirectFacet: unknown;");
    await writeFile(
      path.join(dependency, "index.js"),
      'export { defineFacet as indirectFacet } from "@tembra/sdk";',
    );
    await writeFile(
      path.join(dependency, "node_modules/@tembra/sdk/package.json"),
      JSON.stringify({ name: "@tembra/sdk", type: "module", exports: "./index.js" }),
    );
    await writeFile(
      path.join(dependency, "node_modules/@tembra/sdk/index.js"),
      'export const defineFacet = "unwanted-duplicate-sdk";',
    );
    await writeFile(
      path.join(root, "facet.ts"),
      `
      import { Type, local, defineFacet } from "@tembra/sdk";
      import { indirectFacet } from "indirect";
      const contract = local("counter", { version: 1, input: Type.Object({}), members: {} });
      export default { contract, direct: defineFacet, indirect: indirectFacet };
    `,
    );
    const result = await build(config);
    const { artifact } = required(result.artifacts[0]);
    assert.doesNotMatch(artifact.source, /unwanted-duplicate-sdk/u);
    const namespace: unknown = await import(
      `data:text/javascript;base64,${Buffer.from(artifact.source).toString("base64")}`
    );
    assertRecord(namespace);
    const definition = namespace.default;
    assertRecord(definition);
    assert.equal(definition.direct, definition.indirect);
    const { contract } = definition;
    assertRecord(contract);
    assert.equal(contract.name, "counter");
  });
});

test("browser SDK imports use the canonical exported entry instead of a nested copy", async () => {
  await fixture(async (root, config) => {
    const sdk = path.resolve(
      path.dirname(fileURLToPath(import.meta.resolve("@tembra/sdk"))),
      "../..",
    );
    await mkdir(path.join(root, "node_modules/@tembra"), { recursive: true });
    await symlink(sdk, path.join(root, "node_modules/@tembra/sdk"), "dir");
    const dependency = path.join(root, "node_modules/indirect");
    const nested = path.join(dependency, "node_modules/@tembra/sdk");
    await mkdir(path.join(nested, "browser"), { recursive: true });
    await writeFile(
      path.join(dependency, "package.json"),
      JSON.stringify({
        name: "indirect",
        type: "module",
        exports: { types: "./index.d.ts", default: "./index.js" },
      }),
    );
    await writeFile(path.join(dependency, "index.d.ts"), "export const indirectPort: unknown;");
    await writeFile(
      path.join(dependency, "index.js"),
      'export { createHost as indirectPort } from "@tembra/sdk/browser";',
    );
    await writeFile(
      path.join(nested, "package.json"),
      JSON.stringify({
        name: "@tembra/sdk",
        type: "module",
        exports: { ".": "./index.js", "./browser": "./browser/index.js" },
      }),
    );
    await writeFile(
      path.join(nested, "index.js"),
      'export class ContractError extends Error { marker = "unwanted-duplicate-sdk-browser"; }',
    );
    // Relative SDK imports bypass a resolver that only intercepts the public root package name.
    await writeFile(
      path.join(nested, "browser/index.js"),
      'export { ContractError as createHost } from "../index.js";',
    );
    await writeFile(
      path.join(root, "facet.ts"),
      `
      import { createHost } from "@tembra/sdk/browser";
      import { indirectPort } from "indirect";
      export default { direct: createHost, indirect: indirectPort };
    `,
    );
    const result = await Bun.build({
      entrypoints: [path.join(root, "facet.ts")],
      target: "browser",
      format: "esm",
      plugins: [canonicalSdk()],
    });
    assert.equal(result.success, true, result.logs.map(String).join("\n"));
    const source = await required(result.outputs[0]).text();
    assert.doesNotMatch(source, /unwanted-duplicate-sdk-browser/u);
    const namespace: unknown = await import(
      `data:text/javascript;base64,${Buffer.from(source).toString("base64")}`
    );
    assertRecord(namespace);
    const definition = namespace.default;
    assertRecord(definition);
    assert.equal(definition.direct, definition.indirect);
    // A browser Host constructor uses native module URLs and cannot become a closed embedded artifact.
    await assert.rejects(
      build({ ...config, builds: { browser: { entry: "facet.ts", platform: "browser" } } }),
      (error) =>
        error instanceof ToolingError && error.phase === "artifact" && error.reason === "imports",
    );
    await writeFile(
      path.join(dependency, "index.js"),
      'export { createHost as indirectPort } from "@tembra/sdk/browser/private";',
    );
    await assert.rejects(
      build({ ...config, builds: { browser: { entry: "facet.ts", platform: "browser" } } }),
      phase("bundle"),
    );
  });
});

test("remaining dynamic imports/import.meta and pre-cancelled work never publish", async () => {
  await fixture(async (root, config) => {
    await writeFile(path.join(root, "facet.ts"), "export default import.meta.url;");
    await assert.rejects(
      build(config),
      (error: unknown) => error instanceof ToolingError && error.reason === "imports",
    );
    await writeFile(
      path.join(root, "facet.ts"),
      "export default (specifier: string) => import(specifier);",
    );
    await assert.rejects(
      build(config),
      (error: unknown) => error instanceof ToolingError && error.reason === "imports",
    );
    await assert.rejects(build(config, { signal: AbortSignal.abort("test") }), phase("cancelled"));
    await assert.rejects(readdir(path.join(root, "dist/tembra")), { code: "ENOENT" });
  });
});

test("browser targets honor package browser object and string mappings", async () => {
  await fixture(async (root, config) => {
    const directory = path.join(root, "node_modules/remapped");
    await mkdir(directory, { recursive: true });
    await writeFile(
      path.join(directory, "package.json"),
      JSON.stringify({
        name: "remapped",
        main: "./node.js",
        browser: { "./node.js": "./web.js" },
        types: "./index.d.ts",
      }),
    );
    await writeFile(path.join(directory, "index.d.ts"), "export const value: string;");
    await writeFile(path.join(directory, "node.js"), 'export const value = "server";');
    await writeFile(path.join(directory, "web.js"), 'export const value = "browser";');
    await writeFile(
      path.join(root, "facet.ts"),
      'import { value } from "remapped"; export default { value };',
    );
    const result = await build({
      ...config,
      builds: { browser: { entry: "facet.ts", platform: "browser" } },
    });
    assert.match(required(result.artifacts[0]).artifact.source, /"browser"/u);
    assert.doesNotMatch(required(result.artifacts[0]).artifact.source, /"server"/u);
    await writeFile(path.join(directory, "module.js"), 'export const value = "wrong-module";');
    await writeFile(
      path.join(directory, "package.json"),
      JSON.stringify({
        name: "remapped",
        main: "./node.js",
        module: "./module.js",
        browser: "./web.js",
        types: "./index.d.ts",
      }),
    );
    const stringMapping = await build({
      ...config,
      builds: { browser: { entry: "facet.ts", platform: "browser" } },
    });
    const mapped = required(stringMapping.artifacts[0]);
    assert.match(mapped.artifact.source, /"browser"/u);
    assert.doesNotMatch(mapped.artifact.source, /"wrong-module"|"server"/u);
    assert.ok(mapped.inputs.includes(path.join(directory, "web.js")));
    assert.ok(!mapped.inputs.includes(path.join(directory, "module.js")));
  });
});

function required<T>(value: T | undefined): T {
  if (value === undefined) {
    throw new Error("expected fixture value");
  }
  return value;
}

function assertRecord(value: unknown): asserts value is Record<string, unknown> {
  assert.ok(value !== null && typeof value === "object");
}
