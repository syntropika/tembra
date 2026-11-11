import { afterAll, describe, expect, test } from "bun:test";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import path from "node:path";
import { sourceEntry } from "../src/tooling/build.ts";
import { resolver } from "../src/tooling/resolver.ts";
import { typecheck } from "../src/tooling/typecheck.ts";
import type { Description } from "../src/tooling/wit.ts";

const fixtures = path.join(import.meta.dir, "fixtures");
const directory = await mkdtemp(path.join(fixtures, ".authoring-"));
afterAll(() => rm(directory, { recursive: true, force: true }));

async function check(name: string, source: string): Promise<string> {
  const file = path.join(directory, `${name}.ts`);
  await writeFile(file, source);
  try {
    typecheck(file, directory, new Map(), ["tembra-wasm"]);
    return "";
  } catch (error) {
    return (error as Error).message;
  }
}

const API = `import { orb, type Context, type WitMethods, type WitReference } from "@syntropika/tembra-sdk";
declare const api: {
  read(): number;
  later(): Promise<number>;
  audit(input: number): void;
};
`;

describe("authoring types follow the WIT function's async-ness", () => {
  test("synchronous and async implementations matching WIT are accepted", async () => {
    expect(
      await check(
        "accepted",
        API +
          `export const methods = {
  read: () => 1,
  later: async () => 2,
  audit(_environment: unknown, context: Context) { context.check(); },
} satisfies WitMethods<typeof api, unknown>;
declare const reference: WitReference<typeof api>;
export const value: number = reference.read();
export const pending: Promise<number> = reference.later();
export default orb({ name: "accepted", apiVersion: 1 });`,
      ),
    ).toBe("");
  });

  test("a Promise for a synchronous WIT function is a diagnostic", async () => {
    expect(
      await check(
        "scalar",
        API +
          `export const methods = { read: async () => 1, later: async () => 2, audit() {} } satisfies WitMethods<typeof api, unknown>;`,
      ),
    ).toContain("Promise<number>");
    expect(
      await check(
        "void",
        API +
          `export const methods = { read: () => 1, later: async () => 2, audit: async () => {} } satisfies WitMethods<typeof api, unknown>;`,
      ),
    ).toContain("Promise<void>");
  });

  test("a synchronous dependency method is not a Promise", async () => {
    expect(
      await check(
        "reference",
        API +
          `declare const reference: WitReference<typeof api>; export const value: Promise<number> = reference.read();`,
      ),
    ).toContain("not assignable");
  });
});

const description: Description = {
  imports: [
    {
      binding: "example:integrated/arithmetic@1.0.0",
      contract: "example:integrated/arithmetic@1.0.0",
      type_only: false,
      functions: { double: { name: "double", kind: "freestanding", params: [] } },
    },
    {
      binding: "wasi:filesystem/preopens@0.3.0",
      contract: "wasi:filesystem/preopens@0.3.0",
      type_only: false,
      functions: {},
    },
  ],
  exports: [
    {
      binding: "example:integrated/calculation@1.0.0",
      contract: "example:integrated/calculation@1.0.0",
      type_only: false,
      functions: {
        sum: { name: "sum", kind: "async-freestanding", params: [], result: "s32" },
        checked: { name: "checked", kind: "freestanding", params: [], result: 0 },
        frames: { name: "frames", kind: "async-freestanding", params: [], result: 1 },
        "resource-value": { name: "resource-value", kind: "freestanding", params: [], result: 2 },
      },
    },
    {
      binding: "example:hooks/guard@1.0.0",
      contract: "example:hooks/guard@1.0.0",
      type_only: false,
      functions: { apply: { name: "apply", kind: "freestanding", params: [], result: "s32" } },
    },
  ],
  types: [
    { id: 0, definition: { kind: { result: { ok: "s32", err: "string" } } } },
    { id: 1, definition: { kind: { stream: "u8" } } },
    { id: 2, definition: { kind: { type: "s32" } } },
  ],
  javascriptNames: {},
};

describe("source entry generation", () => {
  test("wraps exports with WIT-derived flags and imports dependencies and hooks", () => {
    const entry = sourceEntry(description, "/author/orb.ts", ["guard"], "/sdk/shim.ts");
    expect(entry.source).toContain(
      'import dependency0 from "example:integrated/arithmetic@1.0.0";',
    );
    expect(entry.source).toContain('import hook0 from "example:hooks/guard@1.0.0";');
    expect(entry.source).toContain(
      'export const calculation = orb.exported("example:integrated/calculation@1.0.0", "calculation", { "sum": "a", "checked": "r", "frames": "as", "resourceValue": "" });',
    );
    expect(entry.source).toContain('hooks: { "guard": { module: hook0, method: "apply" } }');
    expect(entry.source).toContain("export const lifecycle = orb.lifecycle;");
    expect([...entry.imports.direct]).toEqual(["wasi:filesystem/preopens@0.3.0"]);
    expect([...entry.imports.entry].sort()).toEqual([
      "example:hooks/guard@1.0.0",
      "example:integrated/arithmetic@1.0.0",
      "tembra:orb/host@0.1.0",
    ]);
  });

  test("refuses world functions, reserved names and undeclared hooks", () => {
    expect(() =>
      sourceEntry(
        {
          ...description,
          exports: [
            { binding: "run", function: { name: "run", kind: "freestanding", params: [] } },
          ],
        },
        "/a.ts",
        [],
      ),
    ).toThrow("source Orbs export interfaces");
    expect(() =>
      sourceEntry(
        {
          ...description,
          exports: [{ ...description.exports[1]!, binding: "example:x/lifecycle@1.0.0" }],
        },
        "/a.ts",
        [],
      ),
    ).toThrow("reserved");
    expect(() => sourceEntry(description, "/a.ts", ["missing"])).toThrow(
      "Hook missing is not an exported WIT interface",
    );
  });
});

async function bundle(author: string) {
  const file = path.join(directory, `author-${Math.random().toString(36).slice(2)}.ts`);
  await writeFile(file, author);
  const entry = sourceEntry(description, file, ["guard"]);
  const result = await Bun.build({
    entrypoints: ["tembra:entry"],
    root: directory,
    format: "esm",
    target: "browser",
    minify: false,
    plugins: [resolver(directory, entry.source, entry.imports)],
  });
  if (!result.success) throw new AggregateError(result.logs, "bundle failed");
  return result.outputs[0]!.text();
}

describe("closed ES module bundle", () => {
  test("keeps WIT imports external and exports lifecycle and interfaces", async () => {
    const output = await bundle(`import { getDirectories } from "wasi:filesystem/preopens@0.3.0";
import { orb } from "@syntropika/tembra-sdk";
export default orb({ name: "x", apiVersion: 1, services: { calculation: { methods: { sum: async () => getDirectories().length } } } });`);
    const imports = [...output.matchAll(/^import .* from "([^"]+)";$/gm)]
      .map((match) => match[1])
      .sort();
    expect(imports).toEqual([
      "example:hooks/guard@1.0.0",
      "example:integrated/arithmetic@1.0.0",
      "tembra:orb/host@0.1.0",
      "wasi:filesystem/preopens@0.3.0",
    ]);
    expect(output).toMatch(/export \{[^}]*\blifecycle\b[^}]*\}/);
    expect(output).toMatch(/export \{[^}]*\bcalculation\b[^}]*\}/);
    expect(output).not.toContain("__tembraOrb");
    expect(output).not.toContain("__hostAsync");
  });

  test("refuses direct dependency, private and ambient imports", async () => {
    const refused = (source: string) =>
      bundle(source).then(
        () => "",
        (error: AggregateError) => String(error.errors ?? error),
      );
    expect(
      await refused(`import d from "example:integrated/arithmetic@1.0.0"; export default d;`),
    ).toContain("is an Orb dependency");
    expect(await refused(`import h from "tembra:orb/host@0.1.0"; export default h;`)).toContain(
      "private to the Tembra runtime",
    );
    expect(await refused(`import "wasi:sockets/types@0.3.0"; export default 1;`)).toContain(
      "does not import wasi:sockets/types@0.3.0",
    );
    expect(await refused(`import fs from "node:fs"; export default fs;`)).toContain(
      "requires its host runtime",
    );
  });
});

describe("optional synchronous results", () => {
  test("an option result is one synchronous function type", async () => {
    expect(
      await check(
        "option",
        `import { type WitMethods } from "@syntropika/tembra-sdk";
declare const api: { choose(input: number): number | undefined };
export const methods = {
  choose: (_e: unknown, _c: unknown, input: number) => (input > 1 ? input : undefined),
} satisfies WitMethods<typeof api, unknown>;`,
      ),
    ).toBe("");
  });
});
