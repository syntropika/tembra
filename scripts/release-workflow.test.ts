import { expect, test } from "bun:test";
import { execFileSync, spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { checkArtifacts } from "./release/npm.ts";
import { nodePlatforms } from "./release/metadata.ts";

interface Step {
  id?: string;
  uses?: string;
  run?: string;
  with?: Record<string, string | boolean>;
}

interface Job {
  needs?: string | string[];
  steps?: Step[];
  with?: Record<string, string>;
}

const root = fileURLToPath(new URL("../", import.meta.url));
const workflow = Bun.YAML.parse(
  readFileSync(path.join(root, ".github/workflows/release.yml"), "utf8"),
) as {
  jobs: Record<string, Job>;
};

test("mobile builds consume the shared interpreter before packaging", () => {
  const producer = workflow.jobs.interpreter!;
  expect(producer.steps!.some((step) => step.run?.includes("just build-interpreter"))).toBe(true);
  const upload = producer.steps!.find((step) => step.with?.name === "interpreter")!;
  expect(upload.uses).toStartWith("actions/upload-artifact@");
  for (const directory of [
    "engine-node/dist/interpreter.wasm",
    "engine-wasm/dist/interpreter",
    "react-native/native/interpreter",
  ])
    expect(upload.with!.path).toContain(`packages/ts/${directory}`);

  for (const name of ["ios", "android", "package"]) {
    const job = workflow.jobs[name]!;
    expect(job.needs).toContain("interpreter");
    const download = job.steps!.findIndex((step) => step.with?.name === "interpreter");
    const build = job.steps!.findIndex((step) =>
      step.run?.includes(name === "package" ? "scripts/release.ts pack" : `build:${name}`),
    );
    expect(download).toBeGreaterThanOrEqual(0);
    expect(download).toBeLessThan(build);
    expect(job.steps![download]!.with!.path).toBe("packages/ts");
  }
});

test("release builds and quality checks use the candidate commit resolved by validation", () => {
  for (const name of ["interpreter", "node", "ios", "android", "package", "publish"]) {
    const checkout = workflow.jobs[name]!.steps!.find((step) =>
      step.uses?.startsWith("actions/checkout@"),
    )!;
    expect(checkout.with!.ref).toBe("${{ needs.validate.outputs.ref }}");
  }
  expect(workflow.jobs.quality!.with!.source_ref).toBe("${{ needs.validate.outputs.ref }}");
});

test("SDK declaration generation receives the native engine before it executes", () => {
  const steps = workflow.jobs.package!.steps!;
  const download = steps.findIndex((step) => step.with?.pattern === "node-*");
  const build = steps.findIndex((step) => step.run === "bun run --cwd packages/ts/sdk build");
  expect(download).toBeGreaterThanOrEqual(0);
  expect(build).toBeGreaterThan(download);
  expect(steps[download]!.with!.path).toBe("packages/ts/engine-node/dist");
  expect(steps[download]!.with!["merge-multiple"]).toBe(true);
});

test("tag recovery checks the exact tag and main ancestry before version validation", () => {
  const directory = mkdtempSync(path.join(tmpdir(), "tembra-release-tag-"));
  try {
    const git = (...args: string[]): string =>
      execFileSync("git", args, {
        cwd: directory,
        encoding: "utf8",
        stdio: ["ignore", "pipe", "pipe"],
      }).trim();
    git("init", "--initial-branch=main");
    git("config", "user.email", "release-test@example.invalid");
    git("config", "user.name", "release test");
    writeFileSync(path.join(directory, "file"), "base");
    git("add", "file");
    git("commit", "-m", "test: create release fixture");
    git("update-ref", "refs/remotes/origin/main", "HEAD");
    git("tag", "v1.2.3");
    const ref = git("rev-parse", "HEAD");
    const bin = path.join(directory, "bin");
    mkdirSync(bin);
    writeFileSync(path.join(bin, "bun"), '#!/bin/sh\nprintf "%s\\n" "$*" >> "$CALLS"\n', {
      mode: 0o755,
    });
    const run = workflow.jobs.validate!.steps!.find((step) => step.id === "tag")!.run!;
    const output = path.join(directory, "output");
    const calls = path.join(directory, "calls");
    const invoke = (githubRef = "refs/heads/main") =>
      spawnSync("bash", ["-euo", "pipefail", "-c", run], {
        cwd: directory,
        encoding: "utf8",
        env: {
          ...process.env,
          PATH: `${bin}${path.delimiter}${process.env.PATH}`,
          GITHUB_REF: githubRef,
          RELEASE_EVENT: "workflow_dispatch",
          RELEASE_REF: "v1.2.3",
          RELEASE_TAG_INPUT: "v1.2.3",
          GITHUB_OUTPUT: output,
          CALLS: calls,
        },
      });
    expect(invoke().status).toBe(0);
    expect(readFileSync(output, "utf8")).toContain(`ref=${ref}`);
    expect(readFileSync(calls, "utf8")).toBe("scripts/release.ts validate v1.2.3\n");
    rmSync(calls);
    expect(invoke("refs/heads/feature").status).not.toBe(0);
    expect(() => readFileSync(calls)).toThrow();
    writeFileSync(path.join(directory, "file"), "next");
    git("commit", "-am", "test: advance release fixture");
    expect(invoke().status).not.toBe(0);
    expect(() => readFileSync(calls)).toThrow();
    git("tag", "-f", "v1.2.3");
    expect(invoke().status).not.toBe(0);
    expect(() => readFileSync(calls)).toThrow();
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("release packaging refuses a missing or different mobile interpreter", () => {
  const directory = mkdtempSync(path.join(tmpdir(), "tembra-release-assets-"));
  const packages = path.join(directory, "packages/ts");
  const mobile = path.join(packages, "react-native/native/interpreter/tembra/interpreter.wasm");
  try {
    const files = [
      ...nodePlatforms.map((platform) => `engine-node/dist/tembra-${platform}.node`),
      "engine-node/dist/interpreter.wasm",
      "engine-wasm/dist/interpreter/interpreter.wasm",
      "engine-wasm/dist/assets.js",
      "engine-wasm/dist/assets.d.ts",
      "engine-wasm/dist/contracts.js",
      "engine-wasm/dist/contracts_bg.wasm",
      "engine-wasm/dist/tembra.js",
      "engine-wasm/dist/tembra_bg.wasm",
      "sdk/dist/index.js",
      "sdk/dist/node.js",
      "sdk/dist/browser.js",
      "react-native/native/swift/TembraFFI.xcframework/Info.plist",
      "react-native/native/swift/Sources/TembraNative/Generated/TembraNative.swift",
      "react-native/native/maven/dev/tembra/tembra-kotlin/1.2.3/tembra-kotlin-1.2.3.aar",
      "react-native/native/maven/dev/tembra/tembra-kotlin/1.2.3/tembra-kotlin-1.2.3.pom",
    ];
    for (const file of files) {
      const destination = path.join(packages, file);
      mkdirSync(path.dirname(destination), { recursive: true });
      writeFileSync(destination, "qualified interpreter");
    }
    expect(() => checkArtifacts(directory, "1.2.3")).toThrow("react-native/native/interpreter");
    mkdirSync(path.dirname(mobile), { recursive: true });
    writeFileSync(mobile, "different interpreter");
    expect(() => checkArtifacts(directory, "1.2.3")).toThrow("Release interpreter differs");
    writeFileSync(mobile, "qualified interpreter");
    const aar = path.join(
      packages,
      files.find((file) => file.endsWith(".aar"))!,
    );
    rmSync(aar);
    for (const abi of ["arm64-v8a", "armeabi-v7a", "x86", "x86_64"]) {
      const library = path.join(directory, "jni", abi, "libtembra_mobile.so");
      mkdirSync(path.dirname(library), { recursive: true });
      writeFileSync(library, "fixture library");
    }
    execFileSync("zip", ["-qr", aar, "jni"], { cwd: directory });
    expect(() => checkArtifacts(directory, "1.2.3")).not.toThrow();
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});
