import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import path from "node:path";
import { crates } from "./metadata.ts";

/** Resume verified crate publication without replacing an existing version's contents. */
export async function publishCrates(
  root: string,
  directory: string,
  version: string,
): Promise<void> {
  execFileSync(
    "cargo",
    [
      "package",
      "--locked",
      "--workspace",
      "--exclude",
      "tembra-node-binding",
      "--all-features",
      "--no-verify",
    ],
    { cwd: root, stdio: "inherit" },
  );
  const pending = [];
  for (const item of crates(root)) {
    const checksum = createHash("sha256")
      .update(readFileSync(path.join(directory, `${item.name}-${version}.crate`)))
      .digest("hex");
    const rebuilt = path.resolve(
      root,
      process.env.CARGO_TARGET_DIR ?? "target",
      "package",
      `${item.name}-${version}.crate`,
    );
    const actual = createHash("sha256").update(readFileSync(rebuilt)).digest("hex");
    if (actual !== checksum) throw new Error(`Rebuilt bytes differ for ${item.name}@${version}`);
    const response = await fetch(`https://crates.io/api/v1/crates/${item.name}/${version}`, {
      headers: { "User-Agent": "tembra-release (https://github.com/syntropika/tembra)" },
      signal: AbortSignal.timeout(30_000),
    });
    if (response.ok) {
      const existing = (await response.json()) as { version: { checksum: string } };
      if (existing.version.checksum !== checksum)
        throw new Error(`Published bytes differ for ${item.name}@${version}`);
      console.log(`Already published: ${item.name}@${version}`);
    } else if (response.status === 404) pending.push("--package", item.name);
    else throw new Error(`crates.io lookup failed with ${response.status}`);
  }
  // The package job has compiled each standalone archive before credentials are issued.
  if (pending.length)
    execFileSync("cargo", ["publish", "--locked", "--no-verify", "--all-features", ...pending], {
      cwd: root,
      stdio: "inherit",
    });
}
