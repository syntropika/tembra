import { test } from "bun:test";
import assert from "node:assert/strict";
import { mkdtemp, mkdir, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { Type, local } from "@tembra/sdk";

import { build } from "../src/build.js";
import { TextEncoder as ServerEncoder } from "../src/encoding.js";

test("server UTF-8 encoder matches platform scalar replacement and partial writes", () => {
  // Compare complete scalar prefixes independently of the platform encodeInto implementation.
  const expected = new TextEncoder();
  const actual = new ServerEncoder();
  for (const text of [
    "",
    "ascii",
    "caf\u00E9",
    "\u4E2D\u6587",
    "\uD83D\uDE80",
    "a\uD800b\uDC00c",
    "\uD800\uD800\uDC00",
  ]) {
    assert.deepEqual(actual.encode(text), expected.encode(text));
    for (let length = 0; length <= expected.encode(text).length + 2; length += 1) {
      const left = new Uint8Array(length).fill(42);
      const right = new Uint8Array(length).fill(42);
      let prefix = "";
      for (const scalar of text) {
        if (expected.encode(prefix + scalar).length > length) {
          break;
        }
        prefix += scalar;
      }
      const bytes = expected.encode(prefix);
      right.set(bytes);
      assert.deepEqual(actual.encodeInto(text, left), {
        read: prefix.length,
        written: bytes.length,
      });
      assert.deepEqual(left, right);
    }
  }
  assert.equal(actual.encoding, "utf-8");
});

test.each([false, true])("server encoder avoids authored bindings (minify=%s)", async (minify) => {
  const root = await mkdtemp(path.join(tmpdir(), "tembra-encoding-"));
  const platformEncoder = globalThis.TextEncoder;
  try {
    const sdk = path.resolve(
      path.dirname(fileURLToPath(import.meta.resolve("@tembra/sdk"))),
      "../..",
    );
    await mkdir(path.join(root, "node_modules/@tembra"), { recursive: true });
    await symlink(sdk, path.join(root, "node_modules/@tembra/sdk"), "dir");
    await writeFile(path.join(root, "package.json"), '{"type":"module"}');
    await writeFile(
      path.join(root, "authored.ts"),
      'export const TextEncoder = "authored TextEncoder";',
    );
    await writeFile(
      path.join(root, "facet.ts"),
      `import { Type, local } from "@tembra/sdk";
       import { TextEncoder as owned } from "./authored.js";
       const width = () => "authored width";
       const encodeInto = () => "authored encodeInto";
       const __TembraTextEncoder = "authored encoder";
       export default {
         width: width(), encodeInto: encodeInto(), encoder: __TembraTextEncoder, owned,
         same: TextEncoder === TextEncoder, instance: new TextEncoder() instanceof TextEncoder,
         bytes: [...new TextEncoder().encode("caf\\u00E9\\uD83D\\uDE80\\uD800")],
         contract: local("caf\\u00E9\\uD83D\\uDE80", { version: 1, input: Type.Object({}), members: {} }).descriptor,
       };`,
    );
    const result = await build({
      root,
      builds: { server: { entry: "facet.ts", platform: "server", minify } },
    });
    const [output] = result.artifacts;
    assert.ok(output !== undefined);
    const namespace: unknown = await import(
      `data:text/javascript;base64,${Buffer.from(output.artifact.source).toString("base64")}`
    );
    assert.ok(namespace !== null && typeof namespace === "object" && "default" in namespace);
    assert.deepEqual(namespace.default, {
      width: "authored width",
      encodeInto: "authored encodeInto",
      encoder: "authored encoder",
      owned: "authored TextEncoder",
      same: true,
      instance: true,
      bytes: [...new TextEncoder().encode("caf\u00E9\uD83D\uDE80\uD800")],
      contract: local("caf\u00E9\uD83D\uDE80", {
        version: 1,
        input: Type.Object({}),
        members: {},
      }).descriptor,
    });
    assert.equal(globalThis.TextEncoder, platformEncoder);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test.each([false, true])("server facet keeps its own TextEncoder (minify=%s)", async (minify) => {
  const value = await bundledValue(
    `const TextEncoder = "authored TextEncoder";
     const object = { TextEncoder: "authored property" };
     export default { value: TextEncoder, property: object.TextEncoder };`,
    minify,
  );
  assert.deepEqual(value, { value: "authored TextEncoder", property: "authored property" });
});

test.each([false, true])(
  "server facet can use the global encoder without SDK (minify=%s)",
  async (minify) => {
    const value = await bundledValue(
      `export default {
      bytes: [...new TextEncoder().encode("caf\\u00E9\\uD83D\\uDE80")],
      same: TextEncoder === TextEncoder,
      instance: new TextEncoder() instanceof TextEncoder,
    };`,
      minify,
    );
    assert.deepEqual(value, {
      bytes: [...new TextEncoder().encode("caf\u00E9\uD83D\uDE80")],
      same: true,
      instance: true,
    });
  },
);

test("server encoder can be retained solely by an object shorthand", async () => {
  const value = await bundledValue("export default { TextEncoder };", false);
  assert.ok(value !== null && typeof value === "object" && "TextEncoder" in value);
  assert.equal(typeof value.TextEncoder, "function");
});

async function bundledValue(source: string, minify: boolean): Promise<unknown> {
  const root = await mkdtemp(path.join(tmpdir(), "tembra-owned-encoding-"));
  try {
    await writeFile(path.join(root, "package.json"), '{"type":"module"}');
    await writeFile(path.join(root, "facet.ts"), source);
    const result = await build({
      root,
      builds: { server: { entry: "facet.ts", platform: "server", minify } },
    });
    const [output] = result.artifacts;
    assert.ok(output !== undefined);
    const namespace: unknown = await import(
      `data:text/javascript;base64,${Buffer.from(output.artifact.source).toString("base64")}`
    );
    assert.ok(namespace !== null && typeof namespace === "object" && "default" in namespace);
    return namespace.default;
  } finally {
    await rm(root, { recursive: true, force: true });
  }
}
