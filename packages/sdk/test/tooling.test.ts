import { expect, test } from "bun:test";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { build } from "../src/tooling/index.ts";

test("build emits one portable bundle per target and refuses runtime dependency resolution", async () => {
  const root = await mkdtemp(path.join(tmpdir(), "tembra-build-"));
  try {
    await writeFile(path.join(root, "dependency.ts"), "export const value = 42;");
    await writeFile(
      path.join(root, "facet.ts"),
      "import {value} from './dependency.ts'; export default {name:'example',apiVersion:1,services:{example:{methods:{read:()=>value}}}};",
    );
    const artifacts = await build({
      root,
      builds: {
        web: { entry: "facet.ts", platform: "browser" },
        embedded: { entry: "facet.ts", platform: "embedded" },
      },
    });
    expect(artifacts).toHaveLength(2);
    expect(await readFile(artifacts[0]!.path, "utf8")).toContain("42");
    expect(await readFile(artifacts[1]!.path, "utf8")).toContain("__tembraFacet");
    await writeFile(
      path.join(root, "facet.ts"),
      "const name = './dependency.ts'; export default () => import(name);",
    );
    await expect(
      build({
        root,
        builds: { web: { entry: "facet.ts", platform: "browser", typecheck: false } },
      }),
    ).rejects.toThrow();
    await writeFile(path.join(root, "facet.ts"), "import fs from 'node:fs'; export default fs;");
    await expect(
      build({
        root,
        builds: { web: { entry: "facet.ts", platform: "browser", typecheck: false } },
      }),
    ).rejects.toThrow();
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});
