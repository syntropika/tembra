import assert from "node:assert/strict";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { test } from "node:test";

import { createHost } from "@tembra/sdk/node";

await test("the transformed source profile contains compiler effects and defines package condition resolution", async () => {
  await assert.rejects(createHost({ moduleBase: "https://example.invalid/" }), {
    reason: "module_url",
  });
  await assert.rejects(createHost({ moduleBase: "file:///tmp/facets?revision=1" }), {
    reason: "module_url",
  });
  await assert.rejects(createHost({ moduleBase: "http://[" }), { reason: "module_url" });
  const root = await mkdtemp(path.join(tmpdir(), "tembra-source-profile-"));
  const host = await createHost({ moduleBase: root });
  try {
    const directory = path.join(root, "node_modules/conditions");
    await mkdir(directory, { recursive: true });
    await writeFile(
      path.join(directory, "package.json"),
      JSON.stringify({
        name: "conditions",
        type: "module",
        exports: {
          ".": {
            node: { import: "./import.js", require: "./require.cjs" },
            default: "./default.js",
          },
          "./esm-only": { import: "./import.js" },
        },
      }),
    );
    await writeFile(path.join(directory, "import.js"), "export const condition='import';");
    await writeFile(path.join(directory, "require.cjs"), "exports.condition='require';");
    await writeFile(
      path.join(directory, "default.js"),
      "throw new Error('wrong export condition');",
    );
    await writeFile(
      path.join(root, "facet.ts"),
      `
      import {condition} from 'conditions';
      import {condition as fallback} from 'conditions/esm-only';
      import {basename} from 'node:path';
      globalThis.__tembraSourceProfile={condition,fallback,builtin:basename('/a/b'),
        bindings:[typeof require,typeof module,typeof exports,typeof arguments],
        error:Error};
      export default {name:'profile',uses:[],setup(){}};
    `,
    );
    const descriptors = Object.getOwnPropertyDescriptors(Error);
    await host.load("./facet.ts");
    assert.deepEqual(globalThis.__tembraSourceProfile, {
      condition: "require",
      fallback: "import",
      builtin: "b",
      bindings: ["function", "object", "object", "object"],
      error: Error,
    });
    assert.deepEqual(Object.getOwnPropertyDescriptors(Error), descriptors);
    const before = host.diagnostics().loader.source.evaluations;
    await writeFile(
      path.join(root, "await.ts"),
      "await Promise.resolve();export default {name:'await',uses:[],setup(){}};",
    );
    await assert.rejects(host.load("./await.ts"), { reason: "module_evaluation" });
    assert.equal(host.diagnostics().loader.source.evaluations, before + 1);
    assert.equal(host.diagnostics().loader.source.evaluationFailures, 1);
    assert.equal(host.diagnostics().loader.evaluationFailures, 0);
    assert.equal(host.diagnostics().loader.inFlight, null);
  } finally {
    Reflect.deleteProperty(globalThis, "__tembraSourceProfile");
    await host.close();
    await rm(root, { recursive: true, force: true });
  }
});

await test("source size admission covers transitive transforms and refuses whole entry batches before evaluation", async () => {
  const root = await mkdtemp(path.join(tmpdir(), "tembra-source-size-"));
  const host = await createHost({ moduleBase: root });
  try {
    const before = host.diagnostics().loader.source;
    const large = `globalThis.__tembraOversize=true;export const value=1;//${"x".repeat(2_097_152)}`;
    await writeFile(path.join(root, "large.ts"), large);
    await writeFile(
      path.join(root, "entry.ts"),
      "import {value} from './large.ts';export default {name:'entry',uses:[],setup(){void value}};",
    );
    await writeFile(
      path.join(root, "small.ts"),
      "globalThis.__tembraSmall=true;export default {name:'small',uses:[],setup(){}};",
    );
    await assert.rejects(host.reload(["./small.ts", "./large.ts"]), { reason: "source_size" });
    assert.equal(globalThis.__tembraSmall, undefined);
    assert.equal(host.diagnostics().loader.source.evaluations, before.evaluations);
    await assert.rejects(host.load("./entry.ts"), { reason: "source_size" });
    assert.equal(globalThis.__tembraOversize, undefined);
    assert.equal(host.diagnostics().loader.source.evaluations, before.evaluations + 1);
    assert.equal(host.diagnostics().loader.source.modules, before.modules + 1);
    assert.equal(host.diagnostics().bridge.callbacks, 0);
    assert.equal(host.diagnostics().loader.inFlight, null);
    await host.load("./small.ts");
    assert.equal(globalThis.__tembraSmall, true);
  } finally {
    Reflect.deleteProperty(globalThis, "__tembraOversize");
    Reflect.deleteProperty(globalThis, "__tembraSmall");
    await host.close();
    await rm(root, { recursive: true, force: true });
  }
});
