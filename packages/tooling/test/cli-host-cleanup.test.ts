import { test } from "bun:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { existsSync } from "node:fs";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { createInterface } from "node:readline";

const cli = path.resolve(import.meta.dir, "../bin/tembra.ts");
const cases = [
  { failure: "malformed", closeFailure: false },
  { failure: "browser getter", closeFailure: false },
  { failure: "apply getter", closeFailure: false },
  { failure: "malformed", closeFailure: true },
];

test.each(cases)(
  "CLI joins accepted host cleanup after adapter validation fails",
  async (scenario) => {
    const root = await mkdtemp(path.join(tmpdir(), "tembra-cli-host-cleanup-"));
    try {
      await writeFile(path.join(root, "facet.ts"), "export default {};\n");
      await writeFile(
        path.join(root, "tembra.config.ts"),
        'export default { builds: { main: { entry: "facet.ts", platform: "server" } }, dev: { adapter: "adapter.ts" } };',
      );
      await writeFile(
        path.join(root, "adapter.ts"),
        `import { writeFile } from "node:fs/promises";
       import path from "node:path";
       export function createHosts(config) {
         const release = Bun.stdin.text();
         const listener = Bun.serve({ port: 0, fetch() { return new Response("owned listener"); } });
         const server = {
           async apply() { throw new Error("delivery before validation"); },
           async close() {
             console.log("closing:" + listener.url);
             await release;
             await listener.stop(true);
             await writeFile(path.join(config.root, "closed"), "joined");
             ${scenario.closeFailure ? 'throw new Error("owned close failed", { cause: new Error("original cleanup cause") });' : ""}
           },
         };
         if (${JSON.stringify(scenario.failure)} === "browser getter") {
           return { server, get browser() { throw new Error("browser getter refused"); } };
         }
         if (${JSON.stringify(scenario.failure)} === "apply getter") {
           return { server, browser: { get apply() { throw new Error("apply getter refused"); } } };
         }
         return { server, browser: { apply: 42, async close() {
           await writeFile(path.join(config.root, "unvalidated-closed"), "unexpected");
         } } };
       }`,
      );
      const child = spawn(process.execPath, [cli, "dev"], { cwd: root });
      let stderr = "";
      let exited = false;
      child.stderr.on("data", (chunk: Buffer) => {
        stderr += chunk.toString();
      });
      child.once("close", () => {
        exited = true;
      });
      const completed = once(child, "close");
      const lines = createInterface({ input: child.stdout });
      const timeout = setTimeout(() => {
        child.kill("SIGKILL");
      }, 10_000);
      try {
        const announcement: readonly unknown[] = await Promise.race([
          once(lines, "line"),
          completed.then(() => {
            throw new Error(`CLI exited before host cleanup began: ${stderr}`);
          }),
        ]);
        const [line] = announcement;
        assert.ok(typeof line === "string" && line.startsWith("closing:"));
        const address = line.slice("closing:".length);
        const response = await fetch(address);
        assert.equal(await response.text(), "owned listener");
        assert.equal(exited, false);
        assert.equal(stderr, "");
        assert.equal(existsSync(path.join(root, "closed")), false);
        child.stdin.end("release cleanup\n");
        const outcome: readonly unknown[] = await completed;
        assert.equal(outcome[0], 1, stderr);
        assert.equal(await readFile(path.join(root, "closed"), "utf-8"), "joined");
        assert.equal(existsSync(path.join(root, "unvalidated-closed")), false);
        assert.match(stderr, /config\/adapter_failed/u);
        if (scenario.failure === "malformed") {
          assert.match(stderr, /Development browser host must implement apply/u);
        } else {
          assert.ok(stderr.includes(`${scenario.failure} refused`), stderr);
        }
        if (scenario.closeFailure) {
          assert.match(stderr, /owned close failed/u);
          assert.match(stderr, /original cleanup cause/u);
        }
      } finally {
        clearTimeout(timeout);
        lines.close();
        child.kill("SIGKILL");
        await completed;
      }
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  },
);
