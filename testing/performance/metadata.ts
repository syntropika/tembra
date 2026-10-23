import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, readFileSync, readdirSync, statSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../../", import.meta.url));
function hash(filename: string) {
  const bytes = readFileSync(filename);
  return { bytes: bytes.length, sha256: createHash("sha256").update(bytes).digest("hex") };
}

/** Input identity is acquired outside measured intervals, including ignored SDK build outputs. */
export function benchmarkMetadata(artifacts: readonly string[]) {
  const names = execFileSync(
    "git",
    ["ls-files", "--cached", "--others", "--exclude-standard", "-z"],
    { cwd: root, encoding: "utf-8" },
  )
    .split("\0")
    .filter((name) => name !== "");
  const source = Object.fromEntries(
    names
      .filter((name) => statSync(path.join(root, name)).isFile())
      .map((name) => [name, hash(path.join(root, name))]),
  );
  const output: Record<string, ReturnType<typeof hash>> = {};
  function visit(relative: string): void {
    const filename = path.join(root, relative);
    if (statSync(filename).isDirectory()) {
      for (const child of readdirSync(filename).toSorted()) {
        visit(path.join(relative, child));
      }
    } else {
      output[relative] = hash(filename);
    }
  }
  for (const artifact of artifacts) {
    if (!existsSync(path.join(root, artifact))) {
      throw new Error(`benchmark artifact is missing: ${artifact}`);
    }
    visit(artifact);
  }
  return {
    revision: execFileSync("git", ["rev-parse", "HEAD"], { cwd: root, encoding: "utf-8" }).trim(),
    rustc: execFileSync("rustc", ["-Vv"], { cwd: root, encoding: "utf-8" }).trim(),
    source,
    artifacts: output,
    profile: process.env.TEMBRA_BENCH_PROFILE ?? null,
    dependency_locks: {
      cargo: hash(path.join(root, "Cargo.lock")),
      bun: hash(path.join(root, "bun.lock")),
    },
  };
}
