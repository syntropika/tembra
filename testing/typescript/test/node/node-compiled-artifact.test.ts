import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { after, before, test } from "node:test";
import { fileURLToPath } from "node:url";

import { Type, defineFacet, defineHook, local, method, native, opaque } from "@tembra/sdk";
import type { EsmArtifactV1 } from "@tembra/sdk";
import { createHost } from "@tembra/sdk/node";
import { deferredVoid, defined } from "@tembra/testing";

const Bias = local("test.compiled-loader.bias", {
  version: 1,
  input: Type.Object({}),
  members: { read: method(undefined, Type.Integer()) },
});
const Capability = native("test.compiled-loader.capability@1");
const Counter = local("test.compiled-loader.counter", {
  version: 1,
  input: Type.Object({}),
  members: {
    count: Type.Integer(),
    increment: method(undefined, Type.Integer()),
    hook: method(Type.Integer(), Type.Integer()),
    observations: method(undefined, Type.String()),
    wait: method(undefined, Type.Integer()),
    value: opaque(Capability.native),
  },
});
let directory = "";
let artifacts: { target: string; artifact: EsmArtifactV1 }[] = [];
before(async () => {
  directory = await mkdtemp(path.join(tmpdir(), "tembra-compiled-loader-"));
  // oxlint-disable-next-line promise/avoid-new -- execFile exposes physical completion through its callback.
  await new Promise<void>((resolve, reject) => {
    execFile(
      process.env.BUN_EXEC_PATH ?? "bun",
      [fileURLToPath(new URL("fixtures/compiled-loader/build.ts", import.meta.url)), directory],
      { timeout: 120_000 },
      (error) => {
        if (error) {
          reject(new Error("compiled fixture build failed", { cause: error }));
        } else {
          resolve();
        }
      },
    );
  });
  const manifest: unknown = JSON.parse(
    await readFile(path.join(directory, "artifacts.json"), "utf-8"),
  );
  // The controlled tooling fixture produces these records; Host.load validates every artifact below.
  // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- This local build script owns the manifest schema; the real Host validates each executable artifact.
  artifacts = manifest as typeof artifacts;
});
after(async () => {
  await rm(directory, { recursive: true, force: true });
});

for (const target of ["plain", "minified"]) {
  // oxlint-disable-next-line no-await-in-loop -- Both targets use the shared fixture and execute sequentially under the real Node runner.
  await test(
    `real ${target} tooling artifacts preserve the ESM profile and Rust lifecycle`,
    { timeout: 30_000 },
    // oxlint-disable-next-line no-loop-func -- The before hook initializes artifacts before both sequential tests.
    async () => {
      const Hook = defineHook("test.compiled-loader.hook", {
        version: 1,
        strategy: "waterfall",
        input: Type.Integer(),
      });
      const entries = artifacts.filter((entry) => entry.target === target);
      const host = await createHost();
      const capability = {};
      const { promise: entered, resolve: enter } = deferredVoid();
      const { promise: gate, resolve: finish } = deferredVoid();
      globalThis.__tembraCompiledLoaderTest = {
        capability,
        closed: 0,
        async wait() {
          enter();
          await gate;
        },
      };
      const errorDescriptors = Object.getOwnPropertyDescriptors(Error);
      let reference;
      try {
        host.registerHook(Hook);
        host.registerOpaque(Capability);
        await host.install(
          defineFacet({
            name: "compiled-loader-native",
            uses: [],
            setup(_scope, declaration) {
              declaration.serve(Bias, () => ({ state: {}, methods: { read: () => 10 } }));
            },
          }),
        );
        assert.ok(defined(entries[0]).artifact.exports.includes("__esModule"));
        await host.load(defined(entries[0]).artifact, "__esModule");
        assert.equal(host.diagnostics().loader.executionProfile, "tembra-esm-v1");
        assert.equal(host.diagnostics().loader.evaluator, "node-esm");
        assert.ok(
          host
            .diagnostics()
            .generations.some(
              (generation) => generation.sourceHash === defined(entries[0]).artifact.sha256,
            ),
        );
        reference = await host.get(Counter, {});
        const expected = Object.fromEntries(
          [
            "arguments",
            "arrowArguments",
            "require",
            "module",
            "exports",
            "filename",
            "dirname",
            "jitiImport",
            "jitiESMResolve",
          ].map((name) => [name, "undefined"]),
        );
        assert.deepEqual(JSON.parse(await reference.observations()), expected);
        assert.equal(await reference.increment(), 11);
        assert.equal(await reference.hook(10), 11);
        assert.equal(reference.value, capability);
        const pending = reference.wait();
        await entered;
        const reloading = host.reload([defined(entries[1]).artifact]);
        await reloading;
        assert.ok(
          host.diagnostics().generations.some((generation) => generation.physicalCallbacks > 0),
        );
        finish();
        assert.equal(await pending, 1);
        await reloading;
        assert.equal(reference.count.value, 1);
        assert.equal(await reference.increment(), 13);
        assert.equal(await reference.hook(10), 12);
        assert.equal(reference.value, capability);
        assert.deepEqual(Object.getOwnPropertyDescriptors(Error), errorDescriptors);
        reference.release();
        reference = undefined;
        await host.close();
        assert.equal(globalThis.__tembraCompiledLoaderTest.closed, 1);
        assert.equal(host.diagnostics().bridge.callbacks, 0);
        assert.equal(host.diagnostics().bridge.values, 0);
        assert.equal(host.diagnostics().generations.length, 0);
      } finally {
        finish();
        reference?.release();
        await host.close();
        Reflect.deleteProperty(globalThis, "__tembraCompiledLoaderTest");
      }
    },
  );
}
