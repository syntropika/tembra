import { test } from "bun:test";
import assert from "node:assert/strict";
import { mkdir, mkdtemp, readFile, readdir, rm, symlink, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";

import {
  defineConfig,
  inspectTrustedModuleInputs,
  loadConfig,
  resolveConfig,
} from "../src/config.js";
import { ToolingError } from "../src/error.js";

test("typed config resolves paths from project root and is idempotent", () => {
  const config = resolveConfig(
    defineConfig({
      root: "project",
      builds: { main: { entry: "src/main.ts", platform: "server" } },
      codegen: [
        {
          source: { command: ["exporter"], watch: ["src/contract.rs"] },
          typescript: "generated/contract.ts",
        },
      ],
      dev: { adapter: "adapter.ts", debounce: 0 },
    }),
    "/workspace",
  );
  assert.equal(config.root, "/workspace/project");
  assert.equal(config.builds.main?.entry, "/workspace/project/src/main.ts");
  assert.equal(config.codegen[0]?.typescript, "/workspace/project/generated/contract.ts");
  assert.equal(config.dev.adapter, "/workspace/project/adapter.ts");
  assert.deepEqual(resolveConfig(config), config);
});

test("configuration refuses malformed options and authored/output collisions", () => {
  for (const config of [
    { builds: { bad: { entry: "main.ts", platform: "node" } } },
    { builds: { "../bad": { entry: "main.ts", platform: "server" } } },
    { builds: {}, dev: { pollInterval: 0 } },
    {
      builds: {},
      codegen: [
        {
          source: { descriptor: "in.json", module: "in.ts", export: "Contract" },
          typescript: "out.ts",
        },
      ],
    },
    { builds: {}, codegen: [{ source: { descriptor: "in.json" }, descriptor: "in.json" }] },
    { builds: {}, codegen: [{ source: { command: [] }, typescript: "out.ts" }] },
    { builds: {}, codegen: [{ source: { descriptor: "in.json" } }] },
  ]) {
    assert.throws(
      () => {
        Reflect.apply(resolveConfig, undefined, [config]);
      },
      (failure: unknown) => failure instanceof ToolingError && failure.phase === "config",
    );
  }
});

test("config reload freshly evaluates imported modules and removes temporary files", async () => {
  const root = await mkdtemp(path.join(os.tmpdir(), "tembra-config-"));
  try {
    const configFile = path.join(root, "tembra.config.ts");
    const dependency = path.join(root, "target.ts");
    await writeFile(dependency, 'export const platform = "server" as const;');
    await writeFile(
      configFile,
      'import { platform } from "./target.js"; export default { builds: { main: {entry: "main.ts", platform} } };',
    );
    const initial = await loadConfig(configFile);
    assert.equal(initial.builds.main?.platform, "server");
    assert.deepEqual(new Set(initial.configDependencies), new Set([configFile, dependency]));
    await writeFile(dependency, 'export const platform = "browser" as const;');
    const refreshed = await loadConfig(configFile);
    assert.equal(refreshed.builds.main?.platform, "browser");
    const directory = await readdir(root);
    assert.equal(
      directory.some((file) => file.startsWith(".tembra-")),
      false,
    );
    await writeFile(configFile, 'throw new Error("config original cause");');
    await assert.rejects(
      loadConfig(configFile),
      (error) =>
        error instanceof ToolingError &&
        error.cause instanceof Error &&
        error.cause.message === "config original cause",
    );
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("trusted config tracks inlined text and data dependencies across fresh loads", async () => {
  const root = await mkdtemp(path.join(os.tmpdir(), "tembra-config-data-"));
  try {
    const file = path.join(root, "tembra.config.ts");
    const text = path.join(root, "platform.txt");
    const toml = path.join(root, "settings.toml");
    const yaml = path.join(root, "more.yaml");
    const json = path.join(root, "data.json");
    await Promise.all([
      writeFile(text, "server"),
      writeFile(toml, 'entry = "main.ts"'),
      writeFile(yaml, "output: output"),
      writeFile(json, '{"compiler":"tsconfig.json"}'),
      writeFile(
        file,
        'import platform from "./platform.txt"; import settings from "./settings.toml"; import more from "./more.yaml"; import data from "./data.json"; export default { builds: { main: {entry: settings.entry, platform} }, outDir: more.output, tsconfig: data.compiler };',
      ),
    ]);
    const expected = new Set([file, text, toml, yaml, json]);
    assert.deepEqual(new Set(await inspectTrustedModuleInputs(file)), expected);
    const initial = await loadConfig(file);
    assert.deepEqual(new Set(initial.configDependencies), expected);
    assert.equal(initial.builds.main?.platform, "server");
    assert.equal(initial.builds.main?.entry, path.join(root, "main.ts"));
    assert.equal(initial.outDir, path.join(root, "output"));
    await writeFile(text, "browser");
    const next = await loadConfig(file);
    assert.equal(next.builds.main?.platform, "browser");
    assert.deepEqual(new Set(next.configDependencies), expected);
    const files = await readdir(root);
    assert.deepEqual(files.toSorted(), [
      "data.json",
      "more.yaml",
      "platform.txt",
      "settings.toml",
      "tembra.config.ts",
    ]);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test.each(["json", "js", "ts"])(
  "trusted file imports fail safely before evaluation",
  async (extension) => {
    const root = await mkdtemp(path.join(os.tmpdir(), "tembra-config-file-"));
    try {
      const file = path.join(root, "tembra.config.ts");
      const resource = path.join(root, `payload.${extension}`);
      const content = extension === "json" ? '{"value":1}' : "export default 1;";
      await writeFile(resource, content);
      await writeFile(
        file,
        `import resource from "./payload.${extension}" with {type: "file"}; import {writeFileSync} from "node:fs"; writeFileSync(new URL("evaluated", import.meta.url), resource); export default { builds: {} };`,
      );
      await assert.rejects(
        inspectTrustedModuleInputs(file),
        (error: unknown) => error instanceof ToolingError && error.reason === "output",
      );
      await assert.rejects(
        loadConfig(file),
        (error: unknown) => error instanceof ToolingError && error.reason === "output",
      );
      assert.equal(await readFile(resource, "utf-8"), content);
      const files = await readdir(root);
      assert.deepEqual(files.toSorted(), [`payload.${extension}`, "tembra.config.ts"]);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  },
);

test("configuration rejects codegen outputs overlapping imported configuration dependencies", async () => {
  const root = await mkdtemp(path.join(os.tmpdir(), "tembra-config-collision-"));
  try {
    const settings = path.join(root, "settings.ts");
    const authored = "export const builds = {};";
    await writeFile(settings, authored);
    const file = path.join(root, "tembra.config.ts");
    await writeFile(
      file,
      'import { builds } from "./settings.js"; export default { builds, codegen: [{ source: { descriptor: "contract.json" }, typescript: "settings.ts" }] };',
    );
    await assert.rejects(
      loadConfig(file),
      (failure: unknown) => failure instanceof ToolingError && failure.phase === "config",
    );
    assert.equal(await readFile(settings, "utf-8"), authored);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("resolved configuration owns copies of caller-controlled option collections", () => {
  const define = { MODE: '"initial"' };
  const conditions = ["development"];
  const command = ["export-contract"];
  const watch = ["contract.rs"];
  const nativeTypes = { "marker@1": "Marker" };
  const rustCommand = ["rust-bindings"];
  const dev = { pollInterval: 100 };
  const config = resolveConfig(
    {
      builds: { main: { entry: "main.ts", platform: "server", define, conditions } },
      codegen: [
        { source: { command, watch }, typescript: "generated.ts", nativeTypes, rustCommand },
      ],
      dev,
    },
    "/project",
  );
  define.MODE = '"changed"';
  conditions.push("changed");
  command[0] = "changed";
  watch[0] = "changed";
  nativeTypes["marker@1"] = "Changed";
  rustCommand[0] = "changed";
  dev.pollInterval = 1;
  assert.deepEqual(config.builds.main?.define, { MODE: '"initial"' });
  assert.deepEqual(config.builds.main?.conditions, ["development"]);
  assert.deepEqual(config.codegen.at(0)?.source, {
    command: ["export-contract"],
    watch: ["/project/contract.rs"],
  });
  assert.deepEqual(config.codegen.at(0)?.nativeTypes, { "marker@1": "Marker" });
  assert.deepEqual(config.codegen.at(0)?.rustCommand, ["rust-bindings"]);
  assert.equal(config.dev.pollInterval, 100);
});

test("configuration follows symlink parents for duplicate absent outputs without banning aliases", async () => {
  const root = await mkdtemp(path.join(os.tmpdir(), "tembra-config-output-alias-"));
  try {
    await mkdir(path.join(root, "generated"));
    await symlink(path.join(root, "generated"), path.join(root, "alias"), "dir");
    const source = { descriptor: "contract.json" };
    assert.throws(
      () =>
        resolveConfig(
          {
            builds: {},
            codegen: [
              { source, typescript: "generated/future/contract.ts" },
              { source, typescript: "alias/future/contract.ts" },
            ],
          },
          root,
        ),
      (failure: unknown) => failure instanceof ToolingError && failure.phase === "config",
    );
    const allowed = resolveConfig(
      { builds: {}, codegen: [{ source, typescript: "alias/future/contract.ts" }] },
      root,
    );
    assert.equal(allowed.codegen[0]?.typescript, path.join(root, "alias/future/contract.ts"));
    assert.deepEqual(await readdir(path.join(root, "generated")), []);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("loaded configuration protects imported settings reached through a symlink output parent", async () => {
  const root = await mkdtemp(path.join(os.tmpdir(), "tembra-config-input-alias-"));
  try {
    await mkdir(path.join(root, "src"));
    await symlink(path.join(root, "src"), path.join(root, "alias"), "dir");
    const authored = "export const builds = {};";
    const settings = path.join(root, "src/settings.ts");
    await writeFile(settings, authored);
    const file = path.join(root, "tembra.config.ts");
    await writeFile(
      file,
      'import { builds } from "./src/settings.js"; export default { builds, codegen: [{ source: { descriptor: "contract.json" }, typescript: "alias/settings.ts" }] };',
    );
    await assert.rejects(
      loadConfig(file),
      (failure: unknown) => failure instanceof ToolingError && failure.phase === "config",
    );
    assert.equal(await readFile(settings, "utf-8"), authored);
    const retained = await readdir(root);
    assert.deepEqual(retained.toSorted(), ["alias", "src", "tembra.config.ts"]);
    await writeFile(path.join(root, "base.json"), "{}");
    const compilerConfig = '{"extends":"../base.json"}';
    await writeFile(path.join(root, "src/tsconfig.json"), compilerConfig);
    await writeFile(
      file,
      'import { builds } from "./src/settings.js"; export default { builds, codegen: [{ source: { descriptor: "contract.json" }, typescript: "alias/tsconfig.json" }] };',
    );
    await assert.rejects(
      loadConfig(file),
      (failure: unknown) => failure instanceof ToolingError && failure.phase === "config",
    );
    assert.equal(await readFile(path.join(root, "src/tsconfig.json"), "utf-8"), compilerConfig);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});
