import { expect, test } from "bun:test";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { NodeArtifact } from "@syntropika/tembra-engine-node";
import { build } from "../src/tooling/index.ts";
import { typecheck } from "../src/tooling/typecheck.ts";

test("build emits Wasm data and refuses unresolved modules and native runtime dependencies", async () => {
  const root = await mkdtemp(path.join(tmpdir(), "tembra-build-"));
  try {
    await writeFile(
      path.join(root, "contract.wit"),
      "package example:tooling; interface example { read: func() -> u32; } world run { export example; }",
    );
    await writeFile(path.join(root, "dependency.ts"), "export const value = 42;");
    await writeFile(
      path.join(root, "orb.ts"),
      "import {value} from './dependency.ts'; export default {name:'example',apiVersion:1,services:{example:{methods:{read:()=>value}}}} as const;",
    );
    const [artifact] = await build({
      root,
      builds: { example: { entry: "orb.ts", wit: "contract.wit" } },
    });
    const bytes = new Uint8Array(await readFile(artifact!.path));
    expect([...bytes.slice(0, 4)]).toEqual([0, 97, 115, 109]);
    expect(JSON.parse(new NodeArtifact(bytes).description())).toMatchObject({
      name: "example",
      kind: "source",
    });
    await writeFile(
      path.join(root, "orb.ts"),
      "const name = './dependency.ts'; export default () => import(name);",
    );
    await expect(
      build({ root, builds: { example: { entry: "orb.ts", wit: "contract.wit" } } }),
    ).rejects.toThrow();
    await writeFile(path.join(root, "orb.ts"), "import fs from 'node:fs'; export default fs;");
    await expect(
      build({ root, builds: { example: { entry: "orb.ts", wit: "contract.wit" } } }),
    ).rejects.toThrow();
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("source initialization stays inside Wasm and inspection never activates factories", async () => {
  const root = await mkdtemp(path.join(tmpdir(), "tembra-artifact-data-"));
  const marker = `tembra-realm-${crypto.randomUUID()}`;
  Reflect.set(globalThis, marker, "host-only");
  try {
    await writeFile(
      path.join(root, "contract.wit"),
      "package example:data; interface values { read: func() -> u32; } world run { export values; }",
    );
    await writeFile(
      path.join(root, "orb.ts"),
      `if (Reflect.get(globalThis, ${JSON.stringify(marker)}) !== undefined) throw new Error('host realm is visible');
Reflect.set(globalThis, ${JSON.stringify(marker)}, 'guest-only');
export default {name:'data',apiVersion:1,create(){throw new Error('factory must not run during preparation');},services:{values:{methods:{read:()=>42}}}} as const;`,
    );
    const [artifact] = await build({
      root,
      builds: { data: { entry: "orb.ts", wit: "contract.wit" } },
    });
    const loaded = new NodeArtifact(new Uint8Array(await readFile(artifact!.path)));
    expect(JSON.parse(loaded.description())).toMatchObject({ name: "data", kind: "source" });
    expect(Reflect.get(globalThis, marker)).toBe("host-only");
  } finally {
    Reflect.deleteProperty(globalThis, marker);
    await rm(root, { recursive: true, force: true });
  }
});

test("guest typechecking uses available globals and project ambient declarations", async () => {
  const root = await mkdtemp(path.join(tmpdir(), "tembra-guest-types-"));
  try {
    await writeFile(
      path.join(root, "tsconfig.json"),
      JSON.stringify({
        compilerOptions: { lib: ["ES2023", "DOM"], types: ["node"] },
        include: ["*.ts"],
      }),
    );
    await writeFile(path.join(root, "env.d.ts"), "declare const BUILD_VALUE: string;");
    const entry = path.join(root, "orb.ts");
    await writeFile(entry, "export const value=BUILD_VALUE; console.log(value); tembra.wait(1);");
    expect(() => typecheck(entry, root)).not.toThrow();
    for (const source of [
      "setTimeout(() => {}, 1);",
      "Buffer.from('x');",
      "fetch('http://example.test');",
      "new URL('http://example.test');",
    ]) {
      await writeFile(entry, source);
      expect(() => typecheck(entry, root)).toThrow(/Cannot find name/);
    }
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("guest typechecking resolves an inherited config relative to its own directory", async () => {
  const root = await mkdtemp(path.join(tmpdir(), "tembra-parent-config-"));
  try {
    const nested = path.join(root, "apps/orb");
    await mkdir(nested, { recursive: true });
    await mkdir(path.join(root, "types"));
    await mkdir(path.join(root, "shared"));
    await writeFile(
      path.join(root, "tsconfig.json"),
      JSON.stringify({
        compilerOptions: { baseUrl: ".", paths: { "@shared/*": ["shared/*"] } },
        include: ["types/**/*.d.ts"],
      }),
    );
    await writeFile(path.join(root, "types/env.d.ts"), "declare const BUILD_VALUE: number;");
    await writeFile(path.join(root, "shared/value.ts"), "export const base = 1;");
    const entry = path.join(nested, "orb.ts");
    await writeFile(
      entry,
      'import {base} from "@shared/value"; export const value=BUILD_VALUE+base;',
    );
    expect(() => typecheck(entry, nested)).not.toThrow();
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});
