import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import path from "node:path";

export const npmPackages = ["engine-node", "engine-wasm", "sdk", "react-native"];
export const nodePlatforms = [
  "linux-x64",
  "linux-arm64",
  "darwin-x64",
  "darwin-arm64",
  "win32-x64",
  "win32-arm64",
];

interface Crate {
  name: string;
  version: string;
  manifest_path: string;
  publish: string[] | null;
  dependencies: { name: string; path?: string; req: string; kind: string | null }[];
}

/** Reject ambiguous tags and keep prereleases away from npm's latest channel. */
export function releaseVersion(tag: string): { version: string; channel: string } {
  const number = "(?:0|[1-9][0-9]*)";
  const identifier = `(?:${number}|[0-9]*[a-zA-Z-][0-9a-zA-Z-]*)`;
  const pattern = new RegExp(
    `^v(${number}\\.${number}\\.${number}(?:-${identifier}(?:\\.${identifier})*)?)$`,
  );
  const version = pattern.exec(tag)?.[1];
  if (!version)
    throw new Error("Use a canonical vMAJOR.MINOR.PATCH tag, optionally with a prerelease");
  return { version, channel: version.includes("-") ? "next" : "latest" };
}

/** Read one npm manifest from the checkout or a prepared package. */
export function manifest(directory: string): Record<string, any> {
  return JSON.parse(readFileSync(path.join(directory, "package.json"), "utf8"));
}

/** Resolve publishable workspace crates using Cargo's own inheritance rules. */
export function crates(root: string): Crate[] {
  const metadata = JSON.parse(
    execFileSync("cargo", ["metadata", "--locked", "--no-deps", "--format-version", "1"], {
      cwd: root,
      encoding: "utf8",
    }),
  );
  const members = new Set(metadata.workspace_members);
  return metadata.packages.filter(
    (item: Crate & { id: string }) => members.has(item.id) && item.publish?.length !== 0,
  );
}

/** Check every registry package and local dependency against the requested release. */
export function validate(root: string, tag: string): { version: string; channel: string } {
  const release = releaseVersion(tag);
  for (const directory of [
    root,
    ...npmPackages.map((name) => path.join(root, "packages/ts", name)),
  ]) {
    const item = manifest(directory);
    if (directory !== root && item.private) throw new Error(`${item.name} must remain public`);
    if (item.version !== release.version)
      throw new Error(`${item.name} version does not match ${tag}`);
    for (const section of ["dependencies", "optionalDependencies", "peerDependencies"]) {
      for (const [name, version] of Object.entries(item[section] ?? {})) {
        if (name.startsWith("@tembra/") && version !== "workspace:*" && version !== release.version)
          throw new Error(`${item.name} has an inconsistent ${name} dependency`);
      }
    }
  }
  for (const item of crates(root)) {
    if (item.version !== release.version)
      throw new Error(`${item.name} version does not match ${tag}`);
    for (const dependency of item.dependencies) {
      if (dependency.path && dependency.kind !== "dev" && dependency.req !== `=${release.version}`)
        throw new Error(`${item.name} needs an exact registry version for ${dependency.name}`);
    }
  }
  return release;
}
