import { spawnSync } from "node:child_process";
import { mkdirSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

export const root = fileURLToPath(new URL("../", import.meta.url));
export const target = path.resolve(root, process.env.CARGO_TARGET_DIR ?? "target");

/** Run one native build step with the repository toolchain. */
export function run(args: string[], env: NodeJS.ProcessEnv = process.env): void {
  const result = spawnSync(args[0]!, args.slice(1), { cwd: root, stdio: "inherit", env });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`${args[0]} failed with status ${result.status}`);
}

/** Generate one language binding from the pinned Rust interface. */
export function generate(language: "swift" | "kotlin", destination: string): void {
  run(["cargo", "build", "--locked", "-p", "tembra-mobile", "--lib"]);
  const library =
    process.platform === "win32"
      ? "tembra_mobile.dll"
      : process.platform === "darwin"
        ? "libtembra_mobile.dylib"
        : "libtembra_mobile.so";
  mkdirSync(destination, { recursive: true });
  run([
    "cargo",
    "run",
    "--locked",
    "-p",
    "tembra-mobile",
    "--features",
    "bindgen",
    "--bin",
    "tembra-bindgen",
    "--",
    "generate",
    path.join(target, "debug", library),
    "--language",
    language,
    "--out-dir",
    destination,
    "--no-format",
  ]);
}

/** Compile the shared engine for one native target. */
export function compile(architecture: string, env = process.env): void {
  run(["rustup", "target", "add", architecture]);
  run(
    [
      "cargo",
      "build",
      "--locked",
      "-p",
      "tembra-mobile",
      "--lib",
      "--release",
      "--target",
      architecture,
    ],
    env,
  );
}
