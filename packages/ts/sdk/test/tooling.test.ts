import { expect, test } from "bun:test";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { build } from "../src/tooling/index.ts";
import { createHost } from "../src/node.ts";
import { Errors, TembraError } from "../src/index.ts";

test("build emits one portable bundle per target and refuses runtime dependency resolution", async () => {
  const root = await mkdtemp(path.join(tmpdir(), "tembra-build-"));
  try {
    await writeFile(path.join(root, "dependency.ts"), "export const value = 42;");
    await writeFile(
      path.join(root, "orb.ts"),
      "import {value} from './dependency.ts'; export default {name:'example',apiVersion:1,services:{example:{methods:{read:()=>value}}}};",
    );
    const artifacts = await build({
      root,
      builds: {
        web: { entry: "orb.ts", platform: "browser" },
        embedded: { entry: "orb.ts", platform: "embedded" },
      },
    });
    expect(artifacts).toHaveLength(2);
    expect(await readFile(artifacts[0]!.path, "utf8")).toContain("42");
    expect(await readFile(artifacts[1]!.path, "utf8")).toContain("__tembraOrb");
    await writeFile(
      path.join(root, "orb.ts"),
      "const name = './dependency.ts'; export default () => import(name);",
    );
    await expect(
      build({
        root,
        builds: { web: { entry: "orb.ts", platform: "browser", typecheck: false } },
      }),
    ).rejects.toThrow();
    await writeFile(path.join(root, "orb.ts"), "import fs from 'node:fs'; export default fs;");
    await expect(
      build({
        root,
        builds: { web: { entry: "orb.ts", platform: "browser", typecheck: false } },
      }),
    ).rejects.toThrow();
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("a standalone orb bundle preserves structured resource failures", async () => {
  const root = await mkdtemp(path.join(tmpdir(), "tembra-errors-"));
  const host = createHost();
  try {
    const sdk = fileURLToPath(new URL("../src/index.ts", import.meta.url));
    await writeFile(
      path.join(root, "orb.ts"),
      `
      import { orb, Errors, TembraError } from ${JSON.stringify(sdk)};
      export { TembraError as BundledError };
      export default orb({
        name: "failed-effect", apiVersion: 1,
        create(self) {
          self.resource("connection", {
            open() { throw new TembraError(Errors.Invalid.StateValidation); }
          });
          return {};
        }
      });
    `,
    );
    const [artifact] = await build({
      root,
      builds: { node: { entry: "orb.ts", platform: "node", typecheck: false } },
    });
    const bundle = await import(pathToFileURL(artifact!.path).href);
    expect(bundle.BundledError).not.toBe(TembraError);
    await expect(host.install(bundle.default)).rejects.toMatchObject(
      Errors.Invalid.StateValidation,
    );
  } finally {
    await host.close();
    await rm(root, { recursive: true, force: true });
  }
});
