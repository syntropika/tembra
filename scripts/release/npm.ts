import { execFileSync } from "node:child_process";
import { cpSync, existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import { manifest, nodePlatforms, npmPackages } from "./metadata.ts";

/** Require all native outputs before creating an installable release archive. */
export function checkArtifacts(root: string, version: string): void {
  const required = [
    ...nodePlatforms.map((platform) => `engine-node/dist/tembra-${platform}.node`),
    "engine-wasm/dist/tembra.js",
    "engine-wasm/dist/tembra_bg.wasm",
    "sdk/dist/index.js",
    "sdk/dist/node.js",
    "sdk/dist/browser.js",
    "sdk/dist/embedded.js",
    "react-native/native/swift/TembraFFI.xcframework/Info.plist",
    "react-native/native/swift/Sources/TembraNative/Generated/TembraNative.swift",
    `react-native/native/maven/dev/tembra/tembra-kotlin/${version}/tembra-kotlin-${version}.aar`,
    `react-native/native/maven/dev/tembra/tembra-kotlin/${version}/tembra-kotlin-${version}.pom`,
  ];
  for (const file of required) {
    if (!existsSync(path.join(root, "packages/ts", file)))
      throw new Error(`Missing release artifact: ${file}`);
  }
  const aar = path.join(
    root,
    "packages/ts/react-native/native/maven/dev/tembra/tembra-kotlin",
    version,
    `tembra-kotlin-${version}.aar`,
  );
  const entries = execFileSync("unzip", ["-Z1", aar], { encoding: "utf8" }).split("\n");
  for (const abi of ["arm64-v8a", "armeabi-v7a", "x86", "x86_64"]) {
    if (!entries.includes(`jni/${abi}/libtembra_mobile.so`))
      throw new Error(`Android archive is missing ${abi}`);
  }
}

/** Copy only shipped files and resolve workspace requirements in a separate staging tree. */
export function stagePackage(root: string, name: string, output: string, version: string): string {
  const source = path.join(root, "packages/ts", name);
  const destination = path.join(output, "staging", name);
  const item = manifest(source);
  mkdirSync(destination, { recursive: true });
  for (const file of item.files)
    cpSync(path.join(source, file), path.join(destination, file), { recursive: true });
  cpSync(path.join(root, "LICENSE"), path.join(destination, "LICENSE"));
  if (!existsSync(path.join(destination, "README.md")))
    cpSync(path.join(root, "README.md"), path.join(destination, "README.md"));
  delete item.devDependencies;
  delete item.scripts;
  for (const section of ["dependencies", "optionalDependencies", "peerDependencies"]) {
    for (const [dependency, requirement] of Object.entries(item[section] ?? {})) {
      if (typeof requirement === "string" && requirement.startsWith("workspace:")) {
        if (!dependency.startsWith("@tembra/"))
          throw new Error(`Unknown workspace dependency: ${dependency}`);
        item[section][dependency] = version;
      }
    }
  }
  writeFileSync(path.join(destination, "package.json"), `${JSON.stringify(item, null, 2)}\n`);
  return destination;
}

/** Pack the exact staged payloads that will be submitted to npm. */
export function pack(root: string, output: string, version: string): void {
  checkArtifacts(root, version);
  const archives = path.join(output, "npm");
  mkdirSync(archives, { recursive: true });
  const packages = [];
  for (const name of npmPackages) {
    const directory = stagePackage(root, name, output, version);
    const [result] = JSON.parse(
      execFileSync("npm", ["pack", "--ignore-scripts", "--json", "--pack-destination", archives], {
        cwd: directory,
        encoding: "utf8",
      }),
    );
    packages.push({
      name: result.name,
      version,
      filename: result.filename,
      integrity: result.integrity,
    });
  }
  writeFileSync(path.join(archives, "packages.json"), `${JSON.stringify(packages, null, 2)}\n`);
}

/** Publish in dependency order and allow reruns only when the existing bytes match. */
export async function publishNpm(
  directory: string,
  channel: string,
  request: typeof fetch = fetch,
  submit: (archive: string, channel: string) => void = (archive, tag) => {
    execFileSync(
      "npm",
      ["publish", archive, "--access", "public", "--provenance", "--ignore-scripts", "--tag", tag],
      { stdio: "inherit" },
    );
  },
): Promise<void> {
  const packages = JSON.parse(readFileSync(path.join(directory, "packages.json"), "utf8"));
  for (const item of packages) {
    const response = await request(
      `https://registry.npmjs.org/${encodeURIComponent(item.name)}/${item.version}`,
      { signal: AbortSignal.timeout(30_000) },
    );
    if (response.ok) {
      const existing = (await response.json()) as { dist?: { integrity?: string } };
      if (existing.dist?.integrity !== item.integrity)
        throw new Error(`Published bytes differ for ${item.name}@${item.version}`);
      console.log(`Already published: ${item.name}@${item.version}`);
      continue;
    }
    if (response.status !== 404) throw new Error(`npm lookup failed with ${response.status}`);
    submit(path.join(directory, item.filename), channel);
  }
}
