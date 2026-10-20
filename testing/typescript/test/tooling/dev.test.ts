import { test } from "bun:test";
import assert from "node:assert/strict";
import { once } from "node:events";
import { writeFileSync } from "node:fs";
import { mkdtemp, rm, readFile, writeFile, unlink, rename } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { setTimeout as delay } from "node:timers/promises";

import { Type, local } from "@tembra/sdk";
import { loadConfig, dev, ToolingError } from "@tembra/sdk/tooling";
import type { BuiltArtifact, BuildResult, ToolingConfig } from "@tembra/sdk/tooling";
import ts from "typescript";

async function fixture(
  run: (root: string, config: ToolingConfig) => Promise<void>,
  parent = tmpdir(),
): Promise<void> {
  const root = await mkdtemp(path.join(parent, "tembra-dev-"));
  try {
    await writeFile(path.join(root, "package.json"), '{"type":"module"}');
    await writeFile(path.join(root, "dependency.ts"), "export const value = 1;");
    await writeFile(
      path.join(root, "one.ts"),
      'import {value} from "./dependency.js"; export default {value};',
    );
    await writeFile(path.join(root, "two.ts"), "export default {value: 2};");
    await run(root, {
      root,
      builds: {
        one: { entry: "one.ts", platform: "server" },
        two: { entry: "two.ts", platform: "browser" },
      },
      dev: { pollInterval: 15, debounce: 50 },
    });
  } finally {
    await rm(root, { recursive: true, force: true });
  }
}
// These fixtures live below tmpdir(). Keep complete replacements on that filesystem and outside
// every watched fixture root so one logical save cannot expose a separate truncate-to-empty edit.
async function saveFixtureFile(file: string, source: string): Promise<void> {
  const staging = await mkdtemp(path.join(tmpdir(), "tembra-dev-save-"));
  try {
    const replacement = path.join(staging, "replacement");
    await writeFile(replacement, source);
    await rename(replacement, file);
  } finally {
    await rm(staging, { recursive: true, force: true });
  }
}

async function until(predicate: () => boolean): Promise<void> {
  const deadline = Date.now() + 15_000;
  while (!predicate()) {
    if (Date.now() > deadline) {
      throw new Error("Development event did not settle");
    }
    // oxlint-disable-next-line no-await-in-loop -- Probe ordered asynchronous development events until the controlled postcondition is visible.
    await delay(20);
  }
}

test("development selects affected graphs and recovers from repeated edits and deletion", async () => {
  await fixture(async (root, config) => {
    const builds: BuildResult[] = [];
    const errors: Error[] = [];
    const session = await dev(config, {
      onBuild(result) {
        builds.push(result);
      },
      onError(error) {
        errors.push(error);
      },
    });
    try {
      await session.ready;
      assert.equal(builds.length, 1);
      await writeFile(path.join(root, "dependency.ts"), "export const value = 3;");
      await until(() => builds.length >= 2);
      assert.deepEqual(
        builds[1]?.artifacts.map((value) => value.target),
        ["one"],
      );
      await writeFile(path.join(root, "dependency.ts"), "export const value = 4;");
      await until(() =>
        builds.some((result) =>
          result.artifacts.some((value) => value.artifact.source.includes("value = 4")),
        ),
      );
      await unlink(path.join(root, "dependency.ts"));
      await until(() => errors.length > 0);
      const count = builds.length;
      await writeFile(path.join(root, "dependency.ts"), "export const value = 5;");
      await until(() => builds.length > count);
      const latest = builds.at(-1)?.artifacts[0];
      assert.ok(latest);
      assert.match(latest.artifact.source, /value = 5/u);
    } finally {
      await session.close();
    }
    await assert.rejects(session.rebuild(), /closed/u);
  });
});

test("a broken browser target at startup cannot block server updates and recovers independently", async () => {
  await fixture(async (root, initial) => {
    const config = { ...initial, dev: { pollInterval: 60_000 } };
    await writeFile(path.join(root, "two.ts"), "export default {value: missingBrowserValue};");
    const builds: string[][] = [];
    const errors: Error[] = [];
    const delivered: BuiltArtifact[] = [];
    const host = {
      async apply(artifacts: readonly BuiltArtifact[]) {
        delivered.push(...artifacts);
        return { status: "applied" as const };
      },
    };
    const session = await dev(config, {
      hosts: { server: host, browser: host },
      onBuild(result) {
        builds.push(result.artifacts.map((artifact) => artifact.target));
      },
      onError(error) {
        errors.push(error);
      },
    });
    try {
      await session.ready;
      assert.deepEqual(builds, [["one"]]);
      assert.deepEqual(
        delivered.map((artifact) => artifact.target),
        ["one"],
      );
      assert.ok(errors.some((error) => error instanceof ToolingError && error.target === "two"));
      await writeFile(path.join(root, "one.ts"), "export default {value: 7};");
      await session.rebuild(["one.ts"]);
      assert.deepEqual(builds, [["one"], ["one"]]);
      assert.equal(delivered.length, 2);
      assert.match(delivered[1]?.artifact.source ?? "", /value: 7/u);
      await writeFile(path.join(root, "two.ts"), "export default {value: 8};");
      await session.rebuild(["two.ts"]);
      assert.deepEqual(builds.at(-1), ["two"]);
      assert.deepEqual(
        delivered.map((artifact) => artifact.target),
        ["one", "one", "two"],
      );
      const goodBrowser = delivered.at(-1);
      assert.ok(goodBrowser);
      await writeFile(path.join(root, "two.ts"), "export default {value: missingAgain};");
      await writeFile(path.join(root, "one.ts"), "export default {value: 9};");
      await session.rebuild(["one.ts", "two.ts"]);
      assert.deepEqual(builds.at(-1), ["one"]);
      assert.equal(delivered.filter((artifact) => artifact.platform === "browser").length, 1);
      assert.equal(await readFile(goodBrowser.sourcePath, "utf-8"), goodBrowser.artifact.source);
    } finally {
      await session.close();
    }
  });
});

test("newly discovered external graph and codegen inputs establish a baseline without a synthetic rebuild", async () => {
  const external = await mkdtemp(path.join(tmpdir(), "tembra-dev-external-"));
  try {
    await fixture(async (root, config) => {
      const contract = local("tooling.external-watch", {
        version: 1,
        input: Type.Object({}),
        members: {},
      });
      const descriptor = path.join(external, "descriptor.ts");
      const helper = path.join(external, "value.ts");
      await writeFile(
        descriptor,
        `export const descriptor = ${JSON.stringify(contract.descriptor)};`,
      );
      await writeFile(
        path.join(root, "contract.ts"),
        `import {descriptor} from ${JSON.stringify(descriptor.replace(/\.ts$/u, ".js"))}; export const Contract = {descriptor};`,
      );
      await writeFile(helper, "export const value = 1;");
      await writeFile(
        path.join(root, "one.ts"),
        `import {value} from ${JSON.stringify(helper.replace(/\.ts$/u, ".js"))}; export default {value};`,
      );
      const builds: string[][] = [];
      const errors: Error[] = [];
      const session = await dev(
        {
          ...config,
          codegen: [
            {
              source: { module: "contract.ts", export: "Contract" },
              typescript: "generated/contract.ts",
            },
          ],
          dev: { pollInterval: 5, debounce: 15 },
        },
        {
          onBuild(result) {
            builds.push(result.artifacts.map((artifact) => artifact.target));
          },
          onError(error) {
            errors.push(error);
          },
        },
      );
      try {
        await session.ready;
        await delay(250);
        assert.deepEqual(
          builds,
          [["one", "two"]],
          "discovery alone must not enqueue an all-target cycle",
        );
        await saveFixtureFile(helper, "export const value = 2;");
        await until(() => builds.length >= 2);
        await delay(150);
        assert.deepEqual(
          builds,
          [["one", "two"], ["one"]],
          "a real external edit builds only its dependent target once",
        );
        await saveFixtureFile(path.join(root, "two.ts"), "export default {value: 3};");
        await until(() => builds.length >= 3);
        await delay(150);
        assert.deepEqual(builds, [["one", "two"], ["one"], ["two"]]);
        assert.deepEqual(errors, []);
        await unlink(helper);
        await until(() => errors.length > 0);
        assert.equal(
          builds.length,
          3,
          "a missing external dependency preserves the prior artifact",
        );
        await saveFixtureFile(helper, "export const value = 4;");
        await until(() => builds.length >= 4);
        await delay(150);
        assert.deepEqual(builds, [["one", "two"], ["one"], ["two"], ["one"]]);
      } finally {
        await session.close();
      }
    });
  } finally {
    await rm(external, { recursive: true, force: true });
  }
});

test("workspace manifests stay watched from shared discovery through target graph adoption", async () => {
  const workspace = await mkdtemp(path.join(tmpdir(), "tembra-dev-workspace-"));
  const manifest = path.join(workspace, "package.json");
  try {
    await writeFile(manifest, '{"private":true,"version":"1.0.0"}');
    await writeFile(path.join(workspace, "bun.lock"), '{"lockfileVersion":1}\n');
    await fixture(async (root, config) => {
      const builds: string[][] = [];
      const errors: Error[] = [];
      const session = await dev(
        { ...config, dev: { pollInterval: 5, debounce: 15 } },
        {
          onBuild(result) {
            builds.push(result.artifacts.map((artifact) => artifact.target));
          },
          onError(error) {
            errors.push(error);
          },
        },
      );
      try {
        await session.ready;
        await delay(400);
        assert.deepEqual(
          builds,
          [["one", "two"]],
          "new shared manifests cannot transiently disappear while targets compile",
        );
        await saveFixtureFile(manifest, '{"private":true,"version":"1.0.1"}');
        await until(() => builds.length >= 2);
        await delay(150);
        assert.deepEqual(
          builds,
          [
            ["one", "two"],
            ["one", "two"],
          ],
          "a real shared manifest edit still rebuilds every target once",
        );
        await saveFixtureFile(path.join(root, "dependency.ts"), "export const value = 8;");
        await until(() => builds.length >= 3);
        await delay(150);
        assert.deepEqual(builds, [["one", "two"], ["one", "two"], ["one"]]);
        assert.deepEqual(errors, []);
      } finally {
        await session.close();
      }
    }, workspace);
  } finally {
    await rm(workspace, { recursive: true, force: true });
  }
});

test("first dependency discovery verifies an external edit after the compiler read before delivery", async () => {
  const external = await mkdtemp(path.join(tmpdir(), "tembra-dev-discovery-race-"));
  const helper = path.join(external, "value.ts");
  // oxlint-disable-next-line typescript/unbound-method -- Preserve the exact compiler hook for restoration; its implementation is receiver-independent.
  const originalRead = ts.sys.readFile;
  let changed = false;
  try {
    await writeFile(helper, "export const value: number = 1;");
    // Control a real compiler-read boundary: bundling can accept this later invalid TypeScript,
    // so only a semantic verification before first delivery can refuse the stale checked candidate.
    ts.sys.readFile = (file, encoding) => {
      const source = originalRead(file, encoding);
      if (!changed && path.resolve(file) === helper) {
        changed = true;
        writeFileSync(helper, 'export const value: number = "invalid";');
      }
      return source;
    };
    await fixture(async (root) => {
      await writeFile(
        path.join(root, "one.ts"),
        `import {value} from ${JSON.stringify(helper.replace(/\.ts$/u, ".js"))}; export default {value};`,
      );
      const errors: Error[] = [];
      let deliveries = 0;
      const session = await dev(
        {
          root,
          builds: { one: { entry: "one.ts", platform: "server" } },
          dev: { pollInterval: 60_000 },
        },
        {
          hosts: {
            server: {
              async apply() {
                deliveries += 1;
                return { status: "applied" };
              },
            },
          },
          onError(error) {
            errors.push(error);
          },
        },
      );
      try {
        await session.ready;
        assert.equal(
          changed,
          true,
          "the external edit occurred after the compiler consumed its prior content",
        );
        assert.equal(
          deliveries,
          0,
          "an artifact whose new dependency changed during discovery must not install",
        );
        assert.ok(
          errors.some((error) => error instanceof ToolingError && error.phase === "typecheck"),
        );
      } finally {
        await session.close();
      }
    });
  } finally {
    ts.sys.readFile = originalRead;
    await rm(external, { recursive: true, force: true });
  }
});

test("build failure cannot install; reload failure preserves the embedding cause", async () => {
  await fixture(async (root, config) => {
    const original = Object.assign(new Error("Migration refused"), {
      code: "upgrade_conflict",
      reason: "state_schema",
      details: { version: 1 },
    });
    const errors: Error[] = [];
    let deliveries = 0;
    let closed = false;
    await writeFile(path.join(root, "one.ts"), "export default {value: missingName};");
    const session = await dev(config, {
      hosts: {
        server: {
          async apply() {
            deliveries += 1;
            throw original;
          },
          async close() {
            closed = true;
          },
        },
      },
      onError(error) {
        errors.push(error);
      },
    });
    try {
      await session.ready;
      assert.equal(deliveries, 0);
      assert.ok(errors[0] instanceof ToolingError);
      await writeFile(path.join(root, "one.ts"), "export default {value: 9};");
      await session.rebuild(["one.ts"]);
      assert.ok(deliveries >= 1);
      const reload = errors.find(
        (error) => error instanceof ToolingError && error.phase === "reload",
      );
      assert.equal(reload?.cause, original);
    } finally {
      await session.close();
    }
    assert.equal(closed, true);
  });
});

test("changes during a delivery queue another serialized cycle and close cancels owned work", async () => {
  await fixture(async (root, config) => {
    let deliveries = 0;
    const { promise: blocked, resolve: release } = deferred();
    const session = await dev(config, {
      hosts: {
        server: {
          async apply(_artifacts, signal) {
            deliveries += 1;
            await (deliveries === 1 ? blocked : once(signal, "abort"));
            return { status: "applied" };
          },
        },
      },
    });
    try {
      await until(() => deliveries === 1);
      await writeFile(path.join(root, "dependency.ts"), "export const value = 7;");
      const queued = session.rebuild(["dependency.ts"]);
      assert.equal(deliveries, 1);
      release();
      await until(() => deliveries === 2);
      await session.close();
      await queued;
      await session.ready;
      assert.equal(deliveries, 2);
    } finally {
      release();
      await session.close();
    }
  });
});

test("trusted configuration and codegen modules do not watch their own transient files", async () => {
  await fixture(async (root) => {
    const { descriptor } = local("tooling.watch-contract", {
      version: 1,
      input: Type.Object({}),
      members: {},
    });
    await writeFile(
      path.join(root, "descriptor.ts"),
      `export const descriptor = ${JSON.stringify(descriptor)};`,
    );
    await writeFile(
      path.join(root, "contract.ts"),
      'import {descriptor} from "./descriptor.js"; await new Promise(resolve => setTimeout(resolve, 120)); export const Contract = {descriptor};',
    );
    await writeFile(
      path.join(root, "targets.ts"),
      'export const builds = {one:{entry:"one.ts",platform:"server"}};',
    );
    await writeFile(
      path.join(root, "tembra.config.ts"),
      'import {builds} from "./targets.js"; export default {builds,codegen:[{source:{module:"contract.ts",export:"Contract"},typescript:"generated/contract.ts"}],dev:{pollInterval:5,debounce:5}};',
    );
    await writeFile(path.join(root, "one.ts"), "export default {value: missingName};");
    const errors: Error[] = [];
    const session = await dev(await loadConfig(path.join(root, "tembra.config.ts")), {
      onError(error) {
        errors.push(error);
      },
    });
    try {
      await session.ready;
      assert.equal(errors.length, 1);
      await delay(600);
      assert.equal(
        errors.length,
        1,
        "failed targets must wait for an authored input edit before retrying",
      );
      await writeFile(path.join(root, "one.ts"), "export default {value: 4};");
      await session.rebuild(["one.ts"]);
      assert.equal(errors.length, 1);
    } finally {
      await session.close();
    }
  });
});

test("transport failure retains its cause and reports an unknown delivery outcome", async () => {
  await fixture(async (_root, config) => {
    const transport = new Error("The acknowledgement connection closed");
    const errors: Error[] = [];
    const session = await dev(config, {
      hosts: {
        server: {
          async apply() {
            throw transport;
          },
        },
      },
      onError(error) {
        errors.push(error);
      },
    });
    try {
      await session.ready;
      const failure = errors.find(
        (error) => error instanceof ToolingError && error.phase === "reload",
      );
      assert.ok(failure instanceof ToolingError);
      assert.equal(failure.reason, "delivery_failed");
      assert.equal(failure.cause, transport);
      assert.match(failure.message, /unknown/u);
    } finally {
      await session.close();
    }
  });
});

function deferred() {
  let complete!: () => void;
  // oxlint-disable-next-line promise/avoid-new -- The test controls the exact completion of an owned host close.
  const promise = new Promise<void>((resolve) => {
    complete = resolve;
  });
  return { promise, resolve: complete };
}

test("failed initial scanning joins adopted host cleanup and retains its filesystem cause", async () => {
  await fixture(async (root) => {
    const invalidRoot = path.join(root, "not-a-directory");
    await writeFile(invalidRoot, "authored source");
    const started = deferred();
    const release = deferred();
    let closes = 0;
    const host = {
      async apply() {
        return { status: "applied" as const };
      },
      async close() {
        closes += 1;
        started.resolve();
        await release.promise;
      },
    };
    const pending = dev(
      { root: invalidRoot, builds: {} },
      { hosts: { server: host, browser: host } },
    );
    let settled = false;
    const observed = pending.then(
      () => {
        settled = true;
      },
      () => {
        settled = true;
      },
    );
    const refused = assert.rejects(
      pending,
      (error) =>
        error instanceof ToolingError &&
        error.phase === "watch" &&
        error.cause instanceof Error &&
        "code" in error.cause &&
        error.cause.code === "ENOTDIR",
    );
    await started.promise;
    assert.equal(settled, false);
    assert.equal(closes, 1);
    release.resolve();
    await Promise.all([observed, refused]);
  });
});

test("development captures admitted configuration, host methods and delivery data before observers mutate them", async () => {
  await fixture(async (root) => {
    const config = {
      root,
      builds: { one: { entry: "one.ts", platform: "server" as const } },
      dev: { pollInterval: 60_000 },
    };
    const delivered: { target: string; sha: string }[] = [];
    let expected: string | undefined;
    const host = {
      async apply(artifacts: readonly BuiltArtifact[]) {
        for (const artifact of artifacts) {
          delivered.push({ target: artifact.target, sha: artifact.artifact.sha256 });
        }
        return { status: "applied" as const };
      },
    };
    const pending = dev(config, {
      hosts: { server: host },
      onBuild(result) {
        const artifact = result.artifacts.at(0);
        assert.ok(artifact !== undefined);
        expected = artifact.artifact.sha256;
        Reflect.set(artifact, "target", "observer-injected");
        Reflect.set(artifact, "artifact", { ...artifact.artifact, sha256: "0".repeat(64) });
        Reflect.set(result, "artifacts", []);
      },
    });
    Reflect.set(config.builds.one, "entry", "missing.ts");
    host.apply = async () => {
      throw new Error("caller replaced the admitted host callback");
    };
    const session = await pending;
    try {
      await session.ready;
      assert.ok(expected !== undefined);
      assert.deepEqual(delivered, [{ target: "one", sha: expected }]);
    } finally {
      await session.close();
    }
  });
});

test("identical applied artifacts skip installation while pending and refused deliveries remain retryable", async () => {
  await fixture(async (root) => {
    let attempts = 0;
    let mode: "applied" | "pending" | "refused" = "applied";
    const errors: Error[] = [];
    const session = await dev(
      {
        root,
        builds: { one: { entry: "one.ts", platform: "server" } },
        dev: { pollInterval: 60_000 },
      },
      {
        hosts: {
          server: {
            async apply() {
              attempts += 1;
              if (mode === "refused") {
                throw new Error("controlled delivery refusal");
              }
              return mode === "pending"
                ? { status: "pending", message: "not connected" }
                : { status: "applied" };
            },
          },
        },
        onError(error) {
          errors.push(error);
        },
      },
    );
    try {
      await session.ready;
      assert.equal(attempts, 1);
      await session.rebuild();
      assert.equal(attempts, 1);
      await writeFile(path.join(root, "one.ts"), "export default {value: 3};");
      mode = "pending";
      await session.rebuild();
      assert.equal(attempts, 2);
      await session.rebuild();
      assert.equal(attempts, 3);
      mode = "refused";
      await session.rebuild();
      assert.equal(attempts, 4);
      await session.rebuild();
      assert.equal(attempts, 5);
      assert.equal(errors.length, 2);
      mode = "applied";
      await session.rebuild();
      assert.equal(attempts, 6);
      await session.rebuild();
      assert.equal(attempts, 6);
    } finally {
      await session.close();
    }
  });
});
