import { test } from "bun:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { once } from "node:events";
import { copyFile, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const cli = fileURLToPath(new URL("../bin/tembra.ts", import.meta.url));
const standalone = fileURLToPath(new URL("../dist/bin/tembra", import.meta.url));
async function execute(
  args: string[],
  compiled = false,
  environment?: { cwd: string; path: string; executable?: string },
): Promise<{ code: number | null; stdout: string; stderr: string }> {
  // oxlint-disable-next-line promise/avoid-new -- Collect output and exit status from the child process close event, including failed CLI exits.
  return await new Promise((resolve, reject) => {
    const child = spawn(
      compiled ? (environment?.executable ?? standalone) : process.execPath,
      compiled ? args : [cli, ...args],
      environment === undefined
        ? undefined
        : { cwd: environment.cwd, env: { ...process.env, PATH: environment.path } },
    );
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk: Buffer) => {
      stdout += chunk.toString();
    });
    child.stderr.on("data", (chunk: Buffer) => {
      stderr += chunk.toString();
    });
    child.on("error", reject);
    child.on("close", (code) => {
      resolve({ code, stdout, stderr });
    });
  });
}

test("CLI reports usage and retains configuration failure causes", async () => {
  const help = await execute(["--help"]);
  assert.equal(help.code, 0);
  const badArguments = await execute(["build", "--unknown"]);
  assert.equal(badArguments.code, 1);
  assert.match(badArguments.stderr, /config\/usage/u);
  const root = await mkdtemp(path.join(os.tmpdir(), "tembra-cli-"));
  try {
    const config = path.join(root, "tembra.config.ts");
    await writeFile(config, 'throw new Error("original config cause");');
    const failure = await execute(["build", "--config", config]);
    assert.equal(failure.code, 1);
    assert.match(failure.stderr, /original config cause/u);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("source TypeScript and standalone CLI produce checked artifacts without external runtimes", async () => {
  const root = await mkdtemp(path.join(os.tmpdir(), "tembra-cli-build-"));
  try {
    const config = path.join(root, "tembra.config.ts");
    await writeFile(
      config,
      'import {defineConfig} from "@tembra/tooling"; export default defineConfig({ builds: { main: { entry: "main.ts", platform: "server", export: "facet", sourcemap: true }, browser: { entry: "browser.ts", platform: "browser", export: "facet", sourcemap: true } }, codegen: [{ source: { module: "contract.ts", export: "Contract" }, typescript: "generated/counter.ts", name: "Counter" }], outDir: "output" });',
    );
    const dependency = path.join(root, "node_modules/standalone-package");
    await mkdir(dependency, { recursive: true });
    await writeFile(
      path.join(dependency, "package.json"),
      JSON.stringify({
        name: "standalone-package",
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
      'import { value } from "standalone-package"; import { defineFacet, type InputOf } from "@tembra/sdk"; import { Counter } from "./generated/counter.js"; const input: InputOf<typeof Counter> = {name: value}; export const facet = defineFacet({ name: input.name, uses: [Counter], setup() {} });';
    await writeFile(path.join(root, "main.ts"), mainSource);
    await writeFile(
      path.join(root, "browser.ts"),
      'import { value } from "standalone-package"; import { BrowserClient } from "@tembra/sdk/browser"; import { Counter } from "./generated/counter.js"; export const facet = { client: BrowserClient, contract: Counter, value };',
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
      'import { local, Type, method } from "@tembra/sdk"; export const Contract = local("standalone.counter", { version: 1, input: Type.Object({name: Type.String()}), members: { read: method(undefined, Type.Integer()) } });',
    );
    const isolated = path.join(root, "isolated");
    await mkdir(isolated);
    const copiedExecutable = path.join(isolated, "tembra");
    await copyFile(standalone, copiedExecutable);
    let previousStandalone: string[] | undefined;
    let sourceInputs: string[][] | undefined;
    for (const compiled of [false, true, true]) {
      // oxlint-disable-next-line no-await-in-loop -- Each CLI build must publish before its output can be validated.
      const result = await execute(["build", "--config", config], compiled, {
        cwd: isolated,
        path: isolated,
        executable: copiedExecutable,
      });
      assert.equal(result.code, 0, result.stderr);
      assert.match(result.stdout, /main:/u);
      const artifacts = result.stdout.trim().split("\n");
      const mainLine = artifacts.find((line) => line.startsWith("main: "));
      const browserLine = artifacts.find((line) => line.startsWith("browser: "));
      assert.ok(mainLine !== undefined && browserLine !== undefined);
      const artifactPath = mainLine.slice("main: ".length);
      // oxlint-disable-next-line no-await-in-loop -- Inspect the artifact produced by this particular executable before running the next one.
      const artifact: unknown = JSON.parse(await readFile(artifactPath, "utf-8"));
      assert.ok(artifact !== null && typeof artifact === "object" && "format" in artifact);
      assert.equal(artifact.format, "tembra-esm-v1");
      assert.ok("source" in artifact && typeof artifact.source === "string");
      assert.doesNotMatch(artifact.source, /wrong-(?:node|bun)-branch|browser-package/u);
      // All outputs belong to this process and must be read before the next CLI invocation.
      // oxlint-disable-next-line no-await-in-loop -- Compare complete immutable artifacts and their maps from consecutive standalone processes.
      const contents = await Promise.all(
        [artifactPath, browserLine.slice("browser: ".length)].flatMap((output) => [
          readFile(output, "utf-8"),
          readFile(path.join(path.dirname(output), "facet.js.map"), "utf-8"),
        ]),
      );
      const snapshot = contents.map((content) =>
        createHash("sha256").update(content).digest("hex"),
      );
      const inputs = [contents[1], contents[3]].map((content) => {
        assert.ok(content !== undefined);
        return sourceContentDigests(content);
      });
      if (compiled) {
        // Raw SDK modules and dependency branches must match source mode under each target profile.
        assert.deepEqual(inputs, sourceInputs);
      } else {
        sourceInputs = inputs;
      }
      if (compiled) {
        if (previousStandalone !== undefined) {
          assert.deepEqual(snapshot, previousStandalone);
        }
        previousStandalone = snapshot;
      }
      const browserArtifact: unknown = JSON.parse(contents[2] ?? "");
      assert.ok(browserArtifact !== null && typeof browserArtifact === "object");
      assert.ok("source" in browserArtifact && typeof browserArtifact.source === "string");
      // oxlint-disable-next-line no-await-in-loop -- Evaluate this process's browser artifact to verify embedded root/browser SDK linkage.
      const namespace: unknown = await import(
        `data:text/javascript;base64,${Buffer.from(browserArtifact.source).toString("base64")}`
      );
      assert.ok(namespace !== null && typeof namespace === "object" && "facet" in namespace);
      const definition: unknown = namespace.facet;
      assert.ok(definition !== null && typeof definition === "object" && "client" in definition);
      assert.equal(typeof definition.client, "function");
      assert.ok("value" in definition);
      assert.equal(definition.value, "browser-package");
      assert.match(
        // oxlint-disable-next-line no-await-in-loop -- Validate code generation performed by each CLI path before starting the next one.
        await readFile(path.join(root, "generated/counter.ts"), "utf-8"),
        /CounterReference/u,
      );
    }
    await writeFile(
      path.join(root, "main.ts"),
      `${mainSource} const wrong: number = "bad"; void wrong;`,
    );
    const refusal = await execute(["build", "--config", config], true, {
      cwd: isolated,
      path: isolated,
      executable: copiedExecutable,
    });
    assert.equal(refusal.code, 1);
    assert.match(refusal.stderr, /typecheck/u);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("CLI dev loads trusted adapter and awaits adapter close on SIGTERM", async () => {
  const root = await mkdtemp(path.join(os.tmpdir(), "tembra-cli-dev-"));
  try {
    await writeFile(
      path.join(root, "main.ts"),
      'export const facet = { name: "dev-fixture", uses: [], setup() {} };',
    );
    await writeFile(
      path.join(root, "tsconfig.json"),
      JSON.stringify({
        compilerOptions: { target: "ES2022", module: "ESNext", strict: true },
        include: ["main.ts"],
      }),
    );
    await writeFile(
      path.join(root, "adapter.ts"),
      'import { writeFile } from "node:fs/promises"; import path from "node:path"; export function createHosts(config) { return { server: { async apply() { return { status: "applied" }; }, async close() { await writeFile(path.join(config.root, "closed"), "yes"); } } }; }',
    );
    const config = path.join(root, "tembra.config.ts");
    await writeFile(
      config,
      'export default { builds: { main: { entry: "main.ts", platform: "server", export: "facet" } }, dev: { adapter: "adapter.ts" } };',
    );
    const child = spawn(process.execPath, [cli, "dev", "--config", config]);
    let stdout = "";
    let stderr = "";
    child.stderr.on("data", (chunk: Buffer) => {
      stderr += chunk.toString();
    });
    const completed = once(child, "close");
    const timeout = setTimeout(() => {
      child.kill("SIGKILL");
    }, 30_000);
    try {
      child.stdout.on("data", (chunk: Buffer) => {
        stdout += chunk.toString();
        if (stdout.includes("server: applied")) {
          child.kill("SIGTERM");
        }
      });
      const outcome: readonly unknown[] = await completed;
      const code = outcome.at(0);
      assert.equal(code, 0, stderr);
      assert.match(stdout, /server: applied/u);
      assert.equal(await readFile(path.join(root, "closed"), "utf-8"), "yes");
    } finally {
      clearTimeout(timeout);
      child.kill("SIGKILL");
    }
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

/** Compare bundled input code independently of temporary and installation-specific source names. */
function sourceContentDigests(raw: string): string[] {
  const map: unknown = JSON.parse(raw);
  assert.ok(map !== null && typeof map === "object" && "sourcesContent" in map);
  assert.ok(Array.isArray(map.sourcesContent));
  return map.sourcesContent
    .map((content: unknown) => {
      assert.equal(typeof content, "string");
      assert.ok(typeof content === "string");
      return createHash("sha256").update(content).digest("hex");
    })
    .toSorted();
}
