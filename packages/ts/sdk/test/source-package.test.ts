import { expect, test } from "bun:test";
import { cp, mkdir, mkdtemp, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

test("a source-only SDK runs a native host and builds orbs from an external project", async () => {
  const root = await mkdtemp(path.join(tmpdir(), "tembra-source-package-"));
  try {
    const sdk = fileURLToPath(new URL("../", import.meta.url));
    const installed = path.join(root, "node_modules/@syntropika/tembra-sdk");
    await mkdir(installed, { recursive: true });
    await cp(path.join(sdk, "src"), path.join(installed, "src"), { recursive: true });
    await cp(path.join(sdk, "package.json"), path.join(installed, "package.json"));
    await symlink(path.join(sdk, "node_modules"), path.join(installed, "node_modules"), "dir");
    await writeFile(path.join(root, "package.json"), '{"type":"module"}');
    await writeFile(
      path.join(root, "orb.ts"),
      `import { orb } from "@syntropika/tembra-sdk";
       export default orb({ name: "source", apiVersion: 1,
         services: { counter: { methods: { read() { return 42; } } } } });`,
    );
    await writeFile(
      path.join(root, "consumer.ts"),
      `import { createHost } from "@syntropika/tembra-sdk/node";
       import { build, Platform } from "@syntropika/tembra-sdk/tooling";
       import declaration from "./orb.ts";
       const host = createHost();
       try {
         await host.install(declaration);
         const reference = await host.get("counter", "main");
         if (await reference.read() !== 42) throw new Error("Native source import failed");
         reference.release();
         const artifacts = await build({ builds: {
           web: { entry: "orb.ts", platform: Platform.Browser, typecheck: false },
           embedded: { entry: "orb.ts", platform: Platform.Embedded, typecheck: false }
         } });
         if (artifacts.length !== 2) throw new Error("Source orb build failed");
       } finally { await host.close(); }
       console.log("SOURCE_PACKAGE_PASS");`,
    );
    const process = Bun.spawn([globalThis.process.execPath, "consumer.ts"], {
      cwd: root,
      stdout: "pipe",
      stderr: "pipe",
    });
    const [status, stdout, stderr] = await Promise.all([
      process.exited,
      new Response(process.stdout).text(),
      new Response(process.stderr).text(),
    ]);
    expect({ status, stderr }).toEqual({ status: 0, stderr: "" });
    expect(stdout).toContain("SOURCE_PACKAGE_PASS");
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});
