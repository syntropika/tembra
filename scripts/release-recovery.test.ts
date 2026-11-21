import { expect, test } from "bun:test";
import { execFileSync, spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

interface Step {
  name?: string;
  run?: string;
  with?: Record<string, string>;
}

const root = fileURLToPath(new URL("../", import.meta.url));
const workflow = Bun.YAML.parse(
  readFileSync(path.join(root, ".github/workflows/release.yml"), "utf8"),
) as {
  jobs: Record<string, { needs?: string[]; steps: Step[] }>;
};

test("Apple libraries are stripped before Swift links them and before upload", () => {
  const steps = workflow.jobs.ios!.steps;
  const build = steps.findIndex((step) => step.run?.includes("build:ios"));
  const strip = steps.findIndex(
    (step) => step.name === "Remove auxiliary bitcode and debug information from Apple libraries",
  );
  const test = steps.findIndex((step) => step.run?.includes("swift test"));
  const upload = steps.findIndex((step) => step.with?.name === "react-native-ios");
  expect(strip).toBeGreaterThan(build);
  expect(test).toBeGreaterThan(strip);
  expect(upload).toBeGreaterThan(test);
  expect(steps[strip]!.run).toContain("rustup component add llvm-tools");
  expect(steps[strip]!.run).toContain('"$strip" --strip-debug');
  expect(steps[strip]!.run).toContain("--remove-section=__LLVM,__bitcode");
  expect(steps[strip]!.run).toContain("--remove-section=__LLVM,__cmdline");
  expect(steps[strip]!.run).toContain("packages/swift/native/TembraFFI.xcframework");
  expect(steps[strip]!.run).toContain(
    "packages/ts/react-native/native/swift/TembraFFI.xcframework",
  );
});

test("recovery archives pass the workflow size guard before installed consumers and upload", () => {
  const steps = workflow.jobs.package!.steps;
  const check = steps.findIndex(
    (step) => step.name === "Retain published archives and check package sizes",
  );
  const smoke = steps.findIndex((step) => step.run?.includes("scripts/release/smoke.ts"));
  const upload = steps.findIndex(
    (step) => step.with?.name === "npm-release" && !step.with?.["run-id"],
  );
  expect(workflow.jobs.package!.needs).toEqual([
    "validate",
    "interpreter",
    "quality",
    "node",
    "ios",
    "android",
  ]);
  expect(check).toBeGreaterThan(steps.findIndex((step) => step.name === "Pack npm artifacts"));
  expect(smoke).toBeGreaterThan(check);
  expect(upload).toBeGreaterThan(smoke);
  expect(steps[check]!.run).toContain("$GITHUB_WORKFLOW_SHA:scripts/release/archives.ts");
  expect(steps[check]!.run).toContain('archives.ts" check');
});

test("archive reuse requires every prior build gate and matching application sources", () => {
  const directory = mkdtempSync(path.join(tmpdir(), "tembra-release-recovery-"));
  const git = (...args: string[]) =>
    execFileSync("git", args, { cwd: directory, encoding: "utf8", stdio: "pipe" }).trim();
  try {
    git("init", "--quiet", "--initial-branch=main");
    git("config", "user.email", "release-test@example.invalid");
    git("config", "user.name", "release test");
    mkdirSync(path.join(directory, "packages"));
    writeFileSync(path.join(directory, "packages/value.ts"), "export const value = 1;\n");
    writeFileSync(path.join(directory, "justfile"), 'nightly := "nightly-2026-10-15"\n');
    git("add", ".");
    git("commit", "--quiet", "-m", "test: create release source");
    const previous = git("rev-parse", "HEAD");
    git("remote", "add", "origin", directory);
    const bin = path.join(directory, "bin");
    mkdirSync(bin);
    writeFileSync(
      path.join(bin, "gh"),
      '#!/bin/sh\ncase "$*" in *jobs*) cat "$RUNNER_TEMP/jobs.json";; *) cat "$RUNNER_TEMP/run.json";; esac\n',
      { mode: 0o755 },
    );
    const record = {
      repository: { full_name: "example/tembra" },
      path: ".github/workflows/release.yml",
      status: "completed",
      head_sha: previous,
    };
    const jobs = [
      ...["validate", "interpreter", "package", "ios", "android"],
      ...Array.from({ length: 6 }, (_, index) => `node (${index})`),
      ...Array.from({ length: 8 }, (_, index) => `quality / ${index}`),
    ].map((name) => ({ name, conclusion: "success" }));
    jobs.push({ name: "publish", conclusion: "failure" });
    jobs.push({ name: "npm-authentication", conclusion: "skipped" });
    const run = workflow.jobs.validate!.steps.find(
      (step) => step.name === "Qualify previous npm archives before reuse",
    )!.run!;
    const invoke = (oldRun = record, oldJobs = jobs) => {
      writeFileSync(path.join(directory, "run.json"), JSON.stringify(oldRun));
      writeFileSync(path.join(directory, "jobs.json"), JSON.stringify({ jobs: oldJobs }));
      return spawnSync("bash", ["-euo", "pipefail", "-c", run], {
        cwd: directory,
        encoding: "utf8",
        env: {
          ...process.env,
          PATH: `${bin}${path.delimiter}${process.env.PATH}`,
          RUNNER_TEMP: directory,
          GITHUB_REPOSITORY: "example/tembra",
          PREVIOUS_RUN: "123",
        },
      });
    };
    expect(invoke().status).toBe(0);
    expect(invoke({ ...record, path: ".github/workflows/other.yml" }).status).not.toBe(0);
    expect(invoke({ ...record, status: "in_progress" }).status).not.toBe(0);
    for (const refused of [
      jobs.map((job) => (job.name === "ios" ? { ...job, conclusion: "failure" } : job)),
      jobs.filter((job) => job.name !== "node (5)"),
      jobs.filter((job) => job.name !== "quality / 7"),
    ])
      expect(invoke(record, refused).status).not.toBe(0);
    writeFileSync(path.join(directory, "justfile"), 'nightly := "nightly-2099-01-01"\n');
    git("add", "justfile");
    git("commit", "--quiet", "-m", "test: change build inputs");
    expect(invoke().status).not.toBe(0);
    writeFileSync(path.join(directory, "justfile"), 'nightly := "nightly-2026-10-15"\n');
    git("add", "justfile");
    git("commit", "--quiet", "-m", "test: restore build inputs");
    writeFileSync(path.join(directory, "packages/value.ts"), "export const value = 2;\n");
    git("add", "packages/value.ts");
    git("commit", "--quiet", "-m", "test: change application source");
    expect(invoke().status).not.toBe(0);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});
