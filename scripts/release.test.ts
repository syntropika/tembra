import { expect, test } from "bun:test";
import { execFileSync, spawnSync } from "node:child_process";
import {
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  renameSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { manifest, releaseVersion, validate } from "./release/metadata.ts";
import { checkArtifacts, stagePackage } from "./release/npm.ts";
import { packageCrates } from "./release/crates.ts";

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

test("crate packaging excludes private helpers with checkout-only dependencies", () => {
  const root = mkdtempSync(path.join(tmpdir(), "tembra-crate-release-"));
  try {
    writeFileSync(
      path.join(root, "Cargo.toml"),
      '[workspace]\nmembers = ["public", "private"]\nresolver = "2"\n',
    );
    for (const name of ["public", "private"]) {
      mkdirSync(path.join(root, name, "src"), { recursive: true });
      writeFileSync(path.join(root, name, "src/lib.rs"), "pub fn value() -> u32 { 42 }\n");
      writeFileSync(
        path.join(root, name, "Cargo.toml"),
        `[package]\nname = "tembra-release-${name}-fixture"\nversion = "0.1.0"\nedition = "2024"\n` +
          (name === "private"
            ? 'publish = false\n[dependencies]\ntembra-release-public-fixture = { path = "../public" }\n'
            : 'description = "Release qualification fixture"\nlicense = "Apache-2.0"\nrepository = "https://github.com/syntropika/tembra"\n'),
      );
    }
    execFileSync("cargo", ["generate-lockfile", "--offline"], { cwd: root, stdio: "pipe" });
    packageCrates(root, false);
    expect(
      readdirSync(path.join(root, "target/package")).filter((file) => file.endsWith(".crate")),
    ).toEqual(["tembra-release-public-fixture-0.1.0.crate"]);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
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

test("restaging replaces a previous snapshot without following links or touching sources", () => {
  const root = mkdtempSync(path.join(tmpdir(), "tembra-restage-"));
  try {
    const source = path.join(root, "packages/ts/sdk");
    const dist = path.join(source, "dist");
    const mounted = path.join(root, "mounted");
    mkdirSync(dist, { recursive: true });
    mkdirSync(mounted);
    writeFileSync(path.join(root, "LICENSE"), "Apache-2.0");
    writeFileSync(path.join(root, "README.md"), "Release fixture");
    writeFileSync(path.join(mounted, "data.txt"), "kept");
    writeFileSync(
      path.join(source, "package.json"),
      JSON.stringify({ name: "@syntropika/tembra-sdk", version: "0.1.0", files: ["dist"] }),
    );
    writeFileSync(path.join(dist, "removed.js"), "export {};");
    writeFileSync(path.join(dist, "old-name.js"), "export {};");
    const output = path.join(root, "release");
    const staged = stagePackage(root, "sdk", output, "0.1.0");
    // A link left in a previous snapshot must be removed itself, never followed.
    symlinkSync(mounted, path.join(staged, "dist/mounted"), "dir");
    rmSync(path.join(dist, "removed.js"));
    renameSync(path.join(dist, "old-name.js"), path.join(dist, "new-name.js"));
    writeFileSync(path.join(dist, "current.js"), "export {};");
    expect(stagePackage(root, "sdk", output, "0.1.0")).toBe(staged);
    const [packed] = JSON.parse(
      execFileSync("npm", ["pack", "--ignore-scripts", "--dry-run", "--json"], {
        cwd: staged,
        encoding: "utf8",
      }),
    );
    expect(packed.files.map((file: { path: string }) => file.path).sort()).toEqual([
      "LICENSE",
      "README.md",
      "dist/current.js",
      "dist/new-name.js",
      "package.json",
    ]);
    expect(readdirSync(mounted)).toEqual(["data.txt"]);
    expect(readFileSync(path.join(mounted, "data.txt"), "utf8")).toBe("kept");
    expect(readdirSync(dist).sort()).toEqual(["current.js", "new-name.js"]);
    // Staging owns only output/staging/<name>; it refuses names and outputs that overlap sources.
    for (const name of ["", ".", "..", "../sdk", "sdk/../sdk", "a/b"])
      expect(() => stagePackage(root, name, output, "0.1.0")).toThrow("Invalid package name");
    const nested = path.join(root, "nested");
    const nestedRoot = path.join(nested, "staging/sdk");
    mkdirSync(path.join(nestedRoot, "packages/ts"), { recursive: true });
    symlinkSync(source, path.join(nestedRoot, "packages/ts/sdk"), "dir");
    writeFileSync(path.join(nestedRoot, "LICENSE"), "Apache-2.0");
    const linked = path.join(root, "linked");
    mkdirSync(linked);
    symlinkSync(source, path.join(linked, "staging"), "dir");
    for (const [base, overlap] of [
      [root, path.join(source, "build")],
      [nestedRoot, nested],
      [root, linked],
    ] as const)
      expect(() => stagePackage(base, "sdk", overlap, "0.1.0")).toThrow("overlaps");
    expect(readdirSync(source).sort()).toEqual(["dist", "package.json"]);
    expect(readdirSync(dist).sort()).toEqual(["current.js", "new-name.js"]);
    expect(readdirSync(nestedRoot).sort()).toEqual(["LICENSE", "packages"]);
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

test("the changelog skips merge commits, lists each breaking footer and refuses other unconventional commits", () => {
  // `just install-tools` pins git-cliff, so a missing or different binary fails here instead of skipping.
  const version = spawnSync("git-cliff", ["--version"], { encoding: "utf8" });
  if (version.error || version.status !== 0)
    throw new Error("git-cliff is not on PATH: run `just install-tools`, which installs 2.14.2.");
  expect(version.stdout.trim()).toBe("git-cliff 2.14.2");
  const config = fileURLToPath(new URL("../cliff.toml", import.meta.url));
  const base = mkdtempSync(path.join(tmpdir(), "tembra-changelog-"));
  // A caller's GIT_DIR, GIT_WORK_TREE or GIT_INDEX_FILE, as `git rebase -x` sets, would override the
  // fixture's directory, and GIT_CLIFF_* would override cliff.toml; the fixture keeps none of them.
  const environment = (inherited: NodeJS.ProcessEnv) => ({
    ...Object.fromEntries(Object.entries(inherited).filter(([name]) => !name.startsWith("GIT_"))),
    GIT_CONFIG_GLOBAL: "/dev/null",
    GIT_CONFIG_NOSYSTEM: "1",
    GIT_AUTHOR_NAME: "Fixture",
    GIT_AUTHOR_EMAIL: "fixture@example.com",
    GIT_COMMITTER_NAME: "Fixture",
    GIT_COMMITTER_EMAIL: "fixture@example.com",
  });
  // Build a repository with a merge commit, a breaking commit and, last, the commit `last`.
  const fixture = (name: string, last: string, inherited: NodeJS.ProcessEnv = process.env) => {
    const cwd = path.join(base, name);
    mkdirSync(cwd);
    const env = environment(inherited);
    const git = (...args: string[]) => execFileSync("git", args, { cwd, env, stdio: "pipe" });
    git("init", "--quiet", "--initial-branch=main");
    git("commit", "--quiet", "--allow-empty", "-m", "feat: start the fixture");
    git("switch", "--quiet", "-c", "side");
    git("commit", "--quiet", "--allow-empty", "-m", "fix(core): mend the side branch");
    git("switch", "--quiet", "main");
    git("merge", "--quiet", "--no-ff", "-m", "Merge branch 'side'", "side");
    git(
      "commit",
      "--quiet",
      "--allow-empty",
      "-m",
      "feat(sdk)!: rename the entry point",
      "-m",
      "BREAKING CHANGE: `start` is now `run`.\nBREAKING CHANGE: `stop` takes a reason.",
    );
    git("commit", "--quiet", "--allow-empty", "-m", last);
    return spawnSync("git-cliff", ["--config", config, "--unreleased"], {
      cwd,
      env,
      encoding: "utf8",
    });
  };
  try {
    const decoy = path.join(base, "decoy");
    mkdirSync(decoy);
    const inDecoy = (...args: string[]) =>
      execFileSync("git", args, {
        cwd: decoy,
        env: environment(process.env),
        stdio: "pipe",
        encoding: "utf8",
      });
    inDecoy("init", "--quiet", "--initial-branch=main");
    inDecoy("commit", "--quiet", "--allow-empty", "-m", "chore: hold the decoy");
    const decoyRefs = inDecoy("show-ref");
    const rendered = fixture("conventional", "docs: close the fixture", {
      ...process.env,
      GIT_DIR: path.join(decoy, ".git"),
      GIT_WORK_TREE: decoy,
      GIT_INDEX_FILE: path.join(decoy, ".git/index"),
    });
    expect(inDecoy("show-ref")).toBe(decoyRefs);
    expect(rendered.stderr).not.toContain("ERROR");
    expect(rendered.status).toBe(0);
    expect(rendered.stdout).not.toContain("Merge");
    expect(rendered.stdout).toContain("- **core:** mend the side branch");
    expect(rendered.stdout).toContain("- close the fixture");
    expect(rendered.stdout).toMatch(
      /- \*\*sdk:\*\* \*\*breaking:\*\* rename the entry point \(`[0-9a-f]{7}`\)\n\s*\n {2}- `start` is now `run`\.\n {2}- `stop` takes a reason\.\n/,
    );
    // Only a commit with two parents is skipped: a title that starts with "Merge" is still refused.
    for (const [name, last] of [
      ["unconventional", "update the fixture"],
      ["merge-titled", "Merge the two parsers into one"],
    ] as const) {
      const refused = fixture(name, last);
      expect(refused.status).not.toBe(0);
      expect(refused.stderr).toContain("UnconventionalCommitsError(1)");
    }
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});
