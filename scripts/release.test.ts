import { expect, test } from "bun:test";
import { execFileSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { manifest, releaseVersion, validate } from "./release/metadata.ts";
import { checkArtifacts, stagePackage } from "./release/npm.ts";

test("only canonical release tags choose latest; prereleases choose next", () => {
  expect(releaseVersion("v1.2.3")).toEqual({ version: "1.2.3", channel: "latest" });
  expect(releaseVersion("v1.2.3-rc.0")).toEqual({ version: "1.2.3-rc.0", channel: "next" });
  for (const tag of [
    "1.2.3",
    "v01.2.3",
    "v1.2",
    "v1.2.3-01",
    "v1.2.3+build",
    "v1.2.3\n",
    "v1.2.3;echo bad",
  ])
    expect(() => releaseVersion(tag)).toThrow();
});

test("all checked-in registry versions agree and a different tag is rejected", () => {
  const root = fileURLToPath(new URL("../", import.meta.url));
  const version = manifest(root).version;
  expect(validate(root, `v${version}`).version).toBe(version);
  expect(() => validate(root, "v999.0.0")).toThrow("version does not match");
});

test("staged npm archives resolve workspace dependencies without shipping development files", () => {
  const root = mkdtempSync(path.join(tmpdir(), "tembra-release-"));
  try {
    const source = path.join(root, "packages/ts/sdk");
    mkdirSync(path.join(source, "src"), { recursive: true });
    writeFileSync(path.join(root, "LICENSE"), "Apache-2.0");
    writeFileSync(path.join(root, "README.md"), "Release fixture");
    writeFileSync(path.join(source, "src/index.ts"), "export const value = 42;");
    writeFileSync(path.join(source, "build.ts"), "throw Error('must not ship');");
    writeFileSync(
      path.join(source, "package.json"),
      JSON.stringify({
        name: "@syntropika/tembra-sdk",
        version: "0.1.0",
        files: ["src"],
        dependencies: { "@syntropika/tembra-engine-node": "workspace:*" },
        devDependencies: { typescript: "*" },
        scripts: { prepare: "exit 1" },
      }),
    );
    const output = path.join(root, "release");
    const staged = stagePackage(root, "sdk", output, "0.1.0");
    const item = manifest(staged);
    expect(item.dependencies).toEqual({ "@syntropika/tembra-engine-node": "0.1.0" });
    expect(item.devDependencies).toBeUndefined();
    const [packed] = JSON.parse(
      execFileSync("npm", ["pack", "--ignore-scripts", "--json"], {
        cwd: staged,
        encoding: "utf8",
      }),
    );
    expect(packed.files.map((file: { path: string }) => file.path).sort()).toEqual([
      "LICENSE",
      "README.md",
      "package.json",
      "src/index.ts",
    ]);
    expect(readFileSync(path.join(source, "package.json"), "utf8")).toContain("workspace:*");
    expect(() => checkArtifacts(root, "0.1.0")).toThrow("Missing release artifact");
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("publication resumes identical versions and stops on conflicting bytes or registry errors", async () => {
  const { publishNpm } = await import("./release/npm.ts");
  const root = mkdtempSync(path.join(tmpdir(), "tembra-publish-"));
  try {
    writeFileSync(
      path.join(root, "packages.json"),
      JSON.stringify([
        {
          name: "@syntropika/tembra-engine-node",
          version: "0.1.0",
          filename: "engine.tgz",
          integrity: "sha512-engine",
        },
        {
          name: "@syntropika/tembra-sdk",
          version: "0.1.0",
          filename: "sdk.tgz",
          integrity: "sha512-sdk",
        },
      ]),
    );
    const submissions: string[] = [];
    const submit = (archive: string, channel: string) => {
      submissions.push(`${path.basename(archive)}:${channel}`);
    };
    const request = Object.assign(
      async (input: string | URL | Request) =>
        String(input).includes("engine-node")
          ? Response.json({ dist: { integrity: "sha512-engine" } })
          : new Response(null, { status: 404 }),
      { preconnect: fetch.preconnect },
    );
    await publishNpm(root, "next", request, submit);
    expect(submissions).toEqual(["sdk.tgz:next"]);
    submissions.length = 0;
    const conflict = Object.assign(
      async () => Response.json({ dist: { integrity: "sha512-other" } }),
      { preconnect: fetch.preconnect },
    );
    await expect(publishNpm(root, "latest", conflict, submit)).rejects.toThrow(
      "Published bytes differ",
    );
    const unavailable = Object.assign(async () => new Response(null, { status: 503 }), {
      preconnect: fetch.preconnect,
    });
    await expect(publishNpm(root, "latest", unavailable, submit)).rejects.toThrow("lookup failed");
    expect(submissions).toEqual([]);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
