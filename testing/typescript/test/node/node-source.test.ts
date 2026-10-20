import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { rm, writeFile } from "node:fs/promises";
import path from "node:path";
import { test } from "node:test";
import { setTimeout as delay } from "node:timers/promises";
import { pathToFileURL } from "node:url";

import { Type, defineFacet, local, method } from "@tembra/sdk";
import { createHost } from "@tembra/sdk/node";
import { deferredVoid, defined } from "@tembra/testing";

import { Counter, sourceFixture, updateSource, installBias } from "./source-fixture.ts";

await test("Node source graphs reload TypeScript and relative ESM with installed packages on one Rust Host", async () => {
  const root = await sourceFixture();
  const host = await createHost({ moduleBase: root });
  const { promise: started, resolve: entered } = deferredVoid();
  const { promise: gate, resolve: finish } = deferredVoid();
  const capability = {};
  globalThis.__tembraSourceTest = {
    capability,
    closed: 0,
    async wait() {
      entered();
      await gate;
    },
  };
  let reference;
  const errorDescriptors = Object.getOwnPropertyDescriptors(Error);
  try {
    await installBias(host);
    await host.load("./facet.ts");
    reference = await host.get(Counter, {});
    assert.equal(await reference.increment(), 14);
    assert.equal(await reference.hook(10), 11);
    assert.equal(reference.value, capability);
    const pending = reference.wait();
    await started;
    await updateSource(root, 2);
    await host.reload([pathToFileURL(path.join(root, "facet.ts"))]);
    assert.ok(
      host
        .diagnostics()
        .generations.some(
          (generation) => generation.kind === "source" && generation.physicalCallbacks > 0,
        ),
    );
    finish();
    assert.equal(await pending, 1);
    assert.equal(reference.count.value, 1);
    assert.equal(await reference.increment(), 16);
    assert.equal(await reference.hook(10), 12);
    assert.equal(reference.value, capability);
    assert.deepEqual(Object.getOwnPropertyDescriptors(Error), errorDescriptors);
    assert.equal(host.diagnostics().loader.evaluations, 0);
    assert.equal(host.diagnostics().loader.source.evaluations, 2);
    assert.equal(host.diagnostics().loader.source.modules, 8);
    const before = host.diagnostics().bridge.callbacks;
    await writeFile(path.join(root, "bad.ts"), "export default {name:'',uses:[],setup(){}};");
    await assert.rejects(host.reload([{ module: "./bad.ts" }]));
    await delay(5);
    assert.equal(host.diagnostics().bridge.callbacks, before);
    assert.equal(await reference.increment(), 18);
    const source = "export default {name:'artifact-compatibility',uses:[],setup(){}};";
    await host.load({
      format: "tembra-esm-v1",
      identity: "compatibility",
      source,
      exports: ["default"],
      sha256: createHash("sha256").update(source).digest("hex"),
    });
    assert.equal(host.diagnostics().loader.evaluations, 1);
    await assert.rejects(host.load("https://example.invalid/facet.ts"), { reason: "module_url" });
    await assert.rejects(host.load("./missing.ts"), { reason: "module_read" });
    reference.release();
    reference = undefined;
    await host.close();
    assert.equal(globalThis.__tembraSourceTest.closed, 1);
    assert.equal(host.diagnostics().bridge.callbacks, 0);
    assert.equal(host.diagnostics().bridge.values, 0);
    assert.equal(host.diagnostics().generations.length, 0);
  } finally {
    finish();
    reference?.release();
    await host.close();
    Reflect.deleteProperty(globalThis, "__tembraSourceTest");
    await rm(root, { recursive: true, force: true });
  }
});

await test("module delivery supports named exports without artifact restrictions and reloads evaluated facets", async () => {
  const root = await sourceFixture();
  const host = await createHost({ moduleBase: root });
  const filename = process.platform === "win32" ? "named#facet.ts" : "named#?facet.ts";
  const Service = local("test.source.preloaded", {
    version: 1,
    input: Type.Object({}),
    members: { count: Type.Integer(), run: method(undefined, Type.Integer()) },
  });
  const facet = defineFacet({
    name: "preloaded-counter",
    uses: [],
    setup(_scope, declaration) {
      declaration.serve(Service, (_input, self) => {
        const count = self.cell("count", { schema: Type.Integer(), initial: 0 });
        return {
          state: { count },
          methods: {
            run(context) {
              return count.replace(context, count.value + 1).value;
            },
          },
        };
      });
    },
  });
  try {
    await writeFile(
      path.join(root, filename),
      "export const named={name:'named-source',uses:[],setup(){}};export function then(){throw new Error('namespace must not be assimilated')};",
    );
    await host.reload([{ module: `./${filename}`, selected: "named" }]);
    await host.install(facet);
    const reference = await host.get(Service, {});
    assert.equal(await reference.run(), 1);
    const before = host.diagnostics().loader.source.evaluations;
    await host.reload([facet]);
    assert.equal(await reference.run(), 2);
    assert.equal(host.diagnostics().loader.source.evaluations, before);
    reference.release();
  } finally {
    await host.close();
    await rm(root, { recursive: true, force: true });
  }
});

await test("all five portable hook strategies and opaque adapters work through source-evaluated callbacks", async () => {
  const host = await createHost();
  const helpers = import.meta.resolve("@tembra/testing/conformance");
  const root = await sourceFixture();
  const file = path.join(root, "hooks.ts");
  await writeFile(
    file,
    `import {portableHooks,javascriptOpaque,hookBoundaries,hookPhysicalSettlement} from ${JSON.stringify(helpers)};globalThis.__tembraSourceHelpers={portableHooks,javascriptOpaque,hookBoundaries,hookPhysicalSettlement};export default {name:'source-hook-checks',uses:[],setup(){}};`,
  );
  try {
    await host.load(pathToFileURL(file));
    for (const run of Object.values(defined(globalThis.__tembraSourceHelpers))) {
      // oxlint-disable-next-line no-await-in-loop -- Each conformance group owns and cleans its fixtures before the next.
      await run(host);
    }
    Reflect.deleteProperty(globalThis, "__tembraSourceHelpers");
    await host.close();
    assert.equal(host.diagnostics().bridge.callbacks, 0);
    assert.equal(host.diagnostics().bridge.values, 0);
  } finally {
    Reflect.deleteProperty(globalThis, "__tembraSourceHelpers");
    await host.close();
    await rm(root, { recursive: true, force: true });
  }
});
