import { createHash } from "node:crypto";
import { copyFileSync, readFileSync, statSync, writeFileSync } from "node:fs";
import path from "node:path";

const maximumArchiveBytes = 100 * 1024 * 1024;

interface Archive {
  name: string;
  version: string;
  filename: string;
  integrity: string;
}

/** Keep compressed packages within the release budget before any registry submission. */
export function checkArchiveSize(file: string): void {
  const size = statSync(file).size;
  if (size > maximumArchiveBytes)
    throw new Error(
      `${path.basename(file)} is ${size} bytes; release archives must not exceed 100 MiB`,
    );
}

/** Retain qualified archives whose versions are already published with the same integrity. */
export async function reusePublishedArchives(
  directory: string,
  previous: string,
  request: typeof fetch = fetch,
): Promise<void> {
  const current: Archive[] = JSON.parse(
    readFileSync(path.join(directory, "packages.json"), "utf8"),
  );
  const old: Archive[] = JSON.parse(readFileSync(path.join(previous, "packages.json"), "utf8"));
  if (
    current.length !== old.length ||
    new Set(old.map((item) => item.name)).size !== old.length ||
    new Set(current.map((item) => item.name)).size !== current.length
  )
    throw new Error("Previous release archives do not match the current package set");
  const replacements = [];
  for (const item of current) {
    if (path.basename(item.filename) !== item.filename || !item.filename.endsWith(".tgz"))
      throw new Error(`Invalid release archive filename: ${item.filename}`);
    const candidate = old.find((entry) => entry.name === item.name);
    if (!candidate || candidate.version !== item.version)
      throw new Error(`Previous release archive does not match ${item.name}@${item.version}`);
    if (
      path.basename(candidate.filename) !== candidate.filename ||
      !candidate.filename.endsWith(".tgz")
    )
      throw new Error(`Invalid release archive filename: ${candidate.filename}`);
    const file = path.join(previous, candidate.filename);
    const integrity = `sha512-${createHash("sha512").update(readFileSync(file)).digest("base64")}`;
    if (integrity !== candidate.integrity)
      throw new Error(`Previous release archive integrity differs for ${item.name}`);
    const response = await request(
      `https://registry.npmjs.org/${encodeURIComponent(item.name)}/${item.version}`,
      { signal: AbortSignal.timeout(30_000) },
    );
    if (response.status === 404) continue;
    if (!response.ok) throw new Error(`npm lookup failed with ${response.status}`);
    const published = (await response.json()) as {
      name?: string;
      version?: string;
      dist?: { integrity?: string };
    };
    if (
      published.name !== item.name ||
      published.version !== item.version ||
      published.dist?.integrity !== integrity
    )
      throw new Error(`Published bytes differ for ${item.name}@${item.version}`);
    checkArchiveSize(file);
    replacements.push({ item, candidate, file });
  }
  for (const { item, candidate, file } of replacements) {
    copyFileSync(file, path.join(directory, item.filename));
    item.integrity = candidate.integrity;
  }
  writeFileSync(path.join(directory, "packages.json"), `${JSON.stringify(current, null, 2)}\n`);
}

if (import.meta.main) {
  const [command, ...args] = process.argv.slice(2);
  if (command === "check" && args.length) for (const file of args) checkArchiveSize(file);
  else if (command === "reuse" && args.length === 2)
    await reusePublishedArchives(args[0]!, args[1]!);
  else
    throw new Error(
      "Usage: bun archives.ts check <archives...> | reuse <directory> <previous-directory>",
    );
}
