import { cpSync, mkdirSync, rmSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { root, run } from "../../../scripts/native.ts";

const directory = fileURLToPath(new URL("./", import.meta.url));
const native = path.join(directory, "native");
const swift = path.join(root, "packages/swift");
const kotlin = path.join(root, "packages/kotlin");

/** Copy prepared native package files into the React Native archive. */
function stage(source: string, destination: string): void {
  rmSync(destination, { recursive: true, force: true });
  mkdirSync(path.dirname(destination), { recursive: true });
  cpSync(source, destination, { recursive: true });
}

const [platform, ...args] = process.argv.slice(2);
if (platform === "bindings" && args.length === 0) {
  run([process.execPath, path.join(swift, "build.ts"), "bindings"]);
  run([process.execPath, path.join(kotlin, "build.ts"), "bindings"]);
} else if (platform === "ios" && args.length === 0) {
  run([process.execPath, path.join(swift, "build.ts"), "build"]);
  stage(path.join(swift, "Sources"), path.join(native, "swift/Sources"));
  stage(
    path.join(swift, "native/TembraFFI.xcframework"),
    path.join(native, "swift/TembraFFI.xcframework"),
  );
} else if (platform === "android") {
  run([process.execPath, path.join(kotlin, "build.ts"), "android", ...args]);
  stage(
    path.join(kotlin, "build/repository/dev/tembra/tembra-kotlin"),
    path.join(native, "maven/dev/tembra/tembra-kotlin"),
  );
} else
  throw new Error(
    "Usage: bun build.ts <bindings|ios|android> [--abis arm64-v8a,x86_64] [--api 24]",
  );
