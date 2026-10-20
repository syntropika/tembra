import { expect, test } from "bun:test";
import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";

const guard = new URL("../scripts/require-node.ts", import.meta.url).href;
const sentinel = "node-runtime-probe";
const probe = `console.log('${sentinel}:' + process.release.name + ':' + String(process.versions.bun))`;

test("Node acceptance uses the real Node runtime from the inherited PATH", () => {
  const result = spawnSync("node", ["--import", guard, "--input-type=module", "--eval", probe], {
    encoding: "utf-8",
    timeout: 10_000,
  });
  expect(result.error).toBeUndefined();
  expect(result.status, result.stderr).toBe(0);
  expect(result.stdout.trim()).toBe(`${sentinel}:node:undefined`);
});

test("the Node preload refuses Bun's recursive node alias before the probe executes", () => {
  const directory = mkdtempSync(path.join(tmpdir(), "tembra-node-runtime-"));
  try {
    writeFileSync(
      path.join(directory, "package.json"),
      JSON.stringify({
        private: true,
        scripts: {
          probe: "bun run nested",
          nested: `node --import ${JSON.stringify(guard)} --input-type=module --eval ${JSON.stringify(probe)}`,
        },
      }),
    );
    const result = spawnSync(process.execPath, ["--bun", "run", "probe"], {
      cwd: directory,
      encoding: "utf-8",
      timeout: 10_000,
    });
    expect(result.error).toBeUndefined();
    expect(result.status).not.toBeNull();
    expect(result.status).not.toBe(0);
    expect(result.stderr).toContain("Node acceptance requires the Node.js runtime");
    expect(result.stdout).not.toContain(sentinel);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});
