import { copyFileSync, mkdirSync, rmSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { compile, generate, run, target } from "../../scripts/native.ts";

const directory = fileURLToPath(new URL("./", import.meta.url));
const native = path.join(directory, "native");

/** Generate Swift sources independently of any application adapter. */
function bindings(): void {
  const generated = path.join(native, "generated");
  generate("swift", generated);
  const sources = path.join(directory, "Sources/TembraNative/Generated");
  mkdirSync(sources, { recursive: true });
  copyFileSync(
    path.join(generated, "TembraNative.swift"),
    path.join(sources, "TembraNative.swift"),
  );
}

/** Package Rust libraries for iOS devices, simulators and macOS. */
function build(): void {
  if (process.platform !== "darwin")
    throw new Error("The Swift package build requires macOS and Xcode");
  bindings();
  const targets = [
    "aarch64-apple-ios",
    "aarch64-apple-ios-sim",
    "x86_64-apple-ios",
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
  ];
  for (const architecture of targets) {
    compile(architecture, {
      ...process.env,
      IPHONEOS_DEPLOYMENT_TARGET: "15.1",
      MACOSX_DEPLOYMENT_TARGET: "11.0",
    });
  }
  const headers = path.join(native, "headers");
  mkdirSync(headers, { recursive: true });
  copyFileSync(path.join(native, "generated/TembraFFI.h"), path.join(headers, "TembraFFI.h"));
  copyFileSync(
    path.join(native, "generated/TembraFFI.modulemap"),
    path.join(headers, "module.modulemap"),
  );
  const libraries = [path.join(target, targets[0]!, "release/libtembra_mobile.a")];
  for (const [name, architectures] of [
    ["simulator", targets.slice(1, 3)],
    ["macos", targets.slice(3)],
  ] as const) {
    const library = path.join(native, name, "libtembra_mobile.a");
    mkdirSync(path.dirname(library), { recursive: true });
    run([
      "xcrun",
      "lipo",
      "-create",
      ...architectures.map((architecture) =>
        path.join(target, architecture, "release/libtembra_mobile.a"),
      ),
      "-output",
      library,
    ]);
    libraries.push(library);
  }
  const framework = path.join(native, "TembraFFI.xcframework");
  rmSync(framework, { recursive: true, force: true });
  run([
    "xcodebuild",
    "-create-xcframework",
    ...libraries.flatMap((library) => ["-library", library, "-headers", headers]),
    "-output",
    framework,
  ]);
}

const [command, ...args] = process.argv.slice(2);
if (args.length) throw new Error("Unexpected Swift build arguments");
if (command === "bindings") bindings();
else if (command === "build") build();
else throw new Error("Usage: bun packages/swift/build.ts <bindings|build>");
