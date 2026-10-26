import { spawnSync } from "node:child_process";
import { copyFileSync, mkdirSync, rmSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../../../", import.meta.url));
const directory = fileURLToPath(new URL("./", import.meta.url));
const target = path.resolve(root, process.env.CARGO_TARGET_DIR ?? "target");
const output = path.join(directory, "native");

/** Run a build step and preserve its output and failure status. */
function run(args: string[], env: NodeJS.ProcessEnv = process.env): void {
  const result = spawnSync(args[0]!, args.slice(1), { cwd: root, stdio: "inherit", env });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`${args[0]} failed with status ${result.status}`);
}

/** Generate both languages from the same library and pinned UniFFI generator. */
function bindings(): void {
  run(["cargo", "build", "--locked", "-p", "tembra-mobile", "--lib"]);
  const suffix = process.platform === "darwin" ? "dylib" : "so";
  const library = path.join(target, "debug", `libtembra_mobile.${suffix}`);
  for (const language of ["swift", "kotlin"]) {
    const destination = path.join(output, "generated", language);
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
      library,
      "--language",
      language,
      "--out-dir",
      destination,
      "--no-format",
    ]);
  }
}

/** Package device and universal simulator libraries in an XCFramework. */
function ios(): void {
  if (process.platform !== "darwin") throw new Error("The iOS build requires macOS and Xcode");
  bindings();
  const targets = ["aarch64-apple-ios", "aarch64-apple-ios-sim", "x86_64-apple-ios"];
  for (const architecture of targets) {
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
      { ...process.env, IPHONEOS_DEPLOYMENT_TARGET: "15.1" },
    );
  }
  const headers = path.join(output, "headers");
  mkdirSync(headers, { recursive: true });
  const generated = path.join(output, "generated", "swift");
  copyFileSync(path.join(generated, "TembraFFI.h"), path.join(headers, "TembraFFI.h"));
  copyFileSync(path.join(generated, "TembraFFI.modulemap"), path.join(headers, "module.modulemap"));
  const simulator = path.join(output, "simulator", "libtembra_mobile.a");
  mkdirSync(path.dirname(simulator), { recursive: true });
  run([
    "xcrun",
    "lipo",
    "-create",
    ...targets
      .slice(1)
      .map((architecture) => path.join(target, architecture, "release/libtembra_mobile.a")),
    "-output",
    simulator,
  ]);
  const framework = path.join(output, "TembraFFI.xcframework");
  rmSync(framework, { recursive: true, force: true });
  run([
    "xcodebuild",
    "-create-xcframework",
    "-library",
    path.join(target, targets[0]!, "release/libtembra_mobile.a"),
    "-headers",
    headers,
    "-library",
    simulator,
    "-headers",
    headers,
    "-output",
    framework,
  ]);
}

const androidTargets: Record<string, string> = {
  "arm64-v8a": "aarch64-linux-android",
  "armeabi-v7a": "armv7-linux-androideabi",
  x86: "i686-linux-android",
  x86_64: "x86_64-linux-android",
};

/** Build Android shared libraries for selected ABIs using cargo-ndk. */
function android(abis: string[], api: string): void {
  if (!/^\d+$/.test(api) || Number(api) < 24) throw new Error("Android API must be at least 24");
  for (const abi of abis) {
    if (!androidTargets[abi]) throw new Error(`Unsupported Android ABI: ${abi}`);
  }
  bindings();
  for (const abi of abis) run(["rustup", "target", "add", androidTargets[abi]!]);
  run([
    "cargo",
    "ndk",
    "--platform",
    api,
    ...abis.flatMap((abi) => ["--target", abi]),
    "--output-dir",
    path.join(directory, "android/src/main/jniLibs"),
    "build",
    "--locked",
    "-p",
    "tembra-mobile",
    "--lib",
    "--release",
  ]);
}

const [platform, ...args] = process.argv.slice(2);
if (platform === "bindings" && args.length === 0) bindings();
else if (platform === "ios" && args.length === 0) ios();
else if (platform === "android") {
  let api = "24";
  let abis = ["arm64-v8a", "x86_64"];
  for (let index = 0; index < args.length; index++) {
    if (args[index] === "--api" && args[index + 1]) api = args[++index]!;
    else if (args[index] === "--abis" && args[index + 1]) abis = args[++index]!.split(",");
    else throw new Error(`Unknown or incomplete argument: ${args[index]}`);
  }
  android(abis, api);
} else
  throw new Error(
    "Usage: bun build.ts <bindings|ios|android> [--abis arm64-v8a,x86_64] [--api 24]",
  );
