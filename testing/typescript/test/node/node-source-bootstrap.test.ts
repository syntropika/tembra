import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { after, test } from "node:test";
import { pathToFileURL } from "node:url";

import type { EsmArtifactV1 } from "@tembra/sdk";
import { createHost } from "@tembra/sdk/node";

const prior = Object.getOwnPropertyDescriptors(Error);
Object.defineProperty(Error, "prepareStackTrace", {
  value: (_error: Error, frames: NodeJS.CallSite[]) => `application stack: ${frames.length}`,
  writable: false,
  configurable: true,
});
Error.stackTraceLimit = 7;
after(() => {
  if (!Object.hasOwn(prior, "prepareStackTrace")) {
    Reflect.deleteProperty(Error, "prepareStackTrace");
  }
  Object.defineProperties(Error, prior);
});

const artifact = (name: string): EsmArtifactV1 => {
  const source = `export default {name:${JSON.stringify(name)},uses:[],setup(){}};`;
  return {
    format: "tembra-esm-v1",
    identity: name,
    source,
    exports: ["default"],
    sha256: createHash("sha256").update(source).digest("hex"),
  };
};
await test("isolated compiler bootstrap preserves application Error and artifact validator snapshots in either order", async () => {
  const root = await mkdtemp(path.join(tmpdir(), "tembra-bootstrap-"));
  const host = await createHost();
  const descriptors = Object.getOwnPropertyDescriptors(Error);
  try {
    await host.load(artifact("before-source"));
    const filename = path.join(root, "facet.ts");
    await writeFile(
      filename,
      "export default {name:'source-bootstrap',uses:[],setup(){if(Error.stackTraceLimit!==7)throw new Error('foreign Error realm')}};",
    );
    await host.load(pathToFileURL(filename));
    await host.load(artifact("after-source"));
    assert.deepEqual(Object.getOwnPropertyDescriptors(Error), descriptors);
    await host.close();
    assert.equal(host.diagnostics().bridge.callbacks, 0);
  } finally {
    await host.close();
    await rm(root, { recursive: true, force: true });
  }
});
