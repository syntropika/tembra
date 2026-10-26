import { execFileSync } from "node:child_process";
import { copyFileSync, mkdirSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { compile, generate, run, target } from "../../scripts/native.ts";

const directory = fileURLToPath(new URL("./", import.meta.url));
const androidTargets: Record<string, string> = {
  "arm64-v8a": "aarch64-linux-android",
  "armeabi-v7a": "armv7-linux-androideabi",
  x86: "i686-linux-android",
  x86_64: "x86_64-linux-android",
};
const jvmTargets: Record<string, [string, string]> = {
  "x86_64-unknown-linux-gnu": ["linux-x86-64", "libtembra_mobile.so"],
  "aarch64-unknown-linux-gnu": ["linux-aarch64", "libtembra_mobile.so"],
  "x86_64-apple-darwin": ["darwin-x86-64", "libtembra_mobile.dylib"],
  "aarch64-apple-darwin": ["darwin-aarch64", "libtembra_mobile.dylib"],
  "x86_64-pc-windows-msvc": ["win32-x86-64", "tembra_mobile.dll"],
  "aarch64-pc-windows-msvc": ["win32-aarch64", "tembra_mobile.dll"],
};

/** Generate one shared Kotlin API for Android and the JVM. */
function bindings(): void {
  generate("kotlin", path.join(directory, "src/generated/kotlin"));
}

/** Publish one native distribution with its runtime dependencies. */
function publish(platform: "jvm" | "android"): void {
  run([
    process.env.GRADLE ?? "gradle",
    "-p",
    directory,
    ...(platform === "android" ? ["-PtembraAndroid=true"] : []),
    `:${platform}:publishReleasePublicationToPackageRepository`,
  ]);
}

/** Package host libraries as JNA resources without the Android SDK. */
function jvm(args: string[]): void {
  const host = execFileSync("rustc", ["-vV"], { encoding: "utf8" }).match(/^host: (.+)$/m)?.[1];
  let architectures = host ? [host] : [];
  if (args.length) {
    if (args.length !== 2 || args[0] !== "--targets")
      throw new Error("Use --targets with comma-separated Rust targets");
    architectures = args[1]!.split(",");
  }
  if (!architectures.length) throw new Error("Could not determine the Rust host target");
  for (const architecture of architectures) {
    if (!Object.hasOwn(jvmTargets, architecture))
      throw new Error(`Unsupported JVM target: ${architecture}`);
  }
  bindings();
  for (const architecture of architectures) {
    compile(architecture);
    const [prefix, library] = jvmTargets[architecture]!;
    const destination = path.join(directory, "src/generated/resources", prefix);
    mkdirSync(destination, { recursive: true });
    copyFileSync(
      path.join(target, architecture, "release", library),
      path.join(destination, library),
    );
  }
  publish("jvm");
}

/** Package Android shared libraries for the selected ABIs. */
function android(args: string[]): void {
  let api = "24";
  let abis = ["arm64-v8a", "x86_64"];
  for (let index = 0; index < args.length; index++) {
    if (args[index] === "--api" && args[index + 1]) api = args[++index]!;
    else if (args[index] === "--abis" && args[index + 1]) abis = args[++index]!.split(",");
    else throw new Error(`Unknown or incomplete argument: ${args[index]}`);
  }
  if (!/^\d+$/.test(api) || Number(api) < 24) throw new Error("Android API must be at least 24");
  for (const abi of abis) {
    if (!Object.hasOwn(androidTargets, abi)) throw new Error(`Unsupported Android ABI: ${abi}`);
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
    path.join(directory, "src/main/jniLibs"),
    "build",
    "--locked",
    "-p",
    "tembra-mobile",
    "--lib",
    "--release",
  ]);
  publish("android");
}

const [command, ...args] = process.argv.slice(2);
if (command === "bindings" && args.length === 0) bindings();
else if (command === "jvm") jvm(args);
else if (command === "android") android(args);
else
  throw new Error(
    "Usage: bun packages/kotlin/build.ts <bindings|jvm|android> [--targets ...|--abis ...]",
  );
