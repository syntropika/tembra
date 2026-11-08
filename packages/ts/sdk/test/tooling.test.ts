import { expect, test } from "bun:test";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { NodeArtifact } from "@syntropika/tembra-engine-node";
import { build } from "../src/tooling/index.ts";

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

test("preparation and artifact inspection never execute author code in the host realm", async () => {
  const root = await mkdtemp(path.join(tmpdir(), "tembra-artifact-data-"));
  try {
    await writeFile(
      path.join(root, "contract.wit"),
      "package example:data; interface values { read: func() -> u32; } world run { export values; }",
    );
    await writeFile(
      path.join(root, "orb.ts"),
      "throw new Error('author code must not execute'); export default {name:'data',apiVersion:1,services:{values:{methods:{read:()=>42}}}} as const;",
    );
    const [artifact] = await build({
      root,
      builds: { data: { entry: "orb.ts", wit: "contract.wit" } },
    });
    const loaded = new NodeArtifact(new Uint8Array(await readFile(artifact!.path)));
    expect(JSON.parse(loaded.description())).toMatchObject({ name: "data", kind: "source" });
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});
