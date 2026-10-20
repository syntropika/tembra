import { execFileSync } from "node:child_process";
import { copyFileSync, mkdirSync, rmSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../../../", import.meta.url));
const sdk = fileURLToPath(new URL("../", import.meta.url));
const [kind] = process.argv.slice(2);
const debug = process.argv.includes("--debug");
const profile = debug ? "debug" : "release";
const cargo = (args: readonly string[]): void => {
  execFileSync("cargo", args, { cwd: root, stdio: "inherit" });
};
const metadata: unknown = JSON.parse(
  execFileSync("cargo", ["metadata", "--no-deps", "--format-version", "1"], {
    cwd: root,
    encoding: "utf-8",
  }),
);
if (
  metadata === null ||
  typeof metadata !== "object" ||
  !("target_directory" in metadata) ||
  typeof metadata.target_directory !== "string" ||
  metadata.target_directory.length === 0
) {
  throw new TypeError("Cargo metadata must declare a target_directory path");
}
const flags = debug ? [] : ["--release"];
if (kind === "native") {
  const dependencies = execFileSync(
    "cargo",
    ["tree", "--locked", "-p", "tembra-node-binding", "--edges", "normal", "--prefix", "none"],
    { cwd: root, encoding: "utf-8" },
  );
  if (/^(?:tembra-executor-js|deno_core|deno_v8|v8)\s/mu.test(dependencies)) {
    throw new Error(
      "Node binding must use the Node runtime without an embedded JavaScript executor",
    );
  }
  cargo(["build", "--locked", "-p", "tembra-node-binding", ...flags]);
  const libraries: Readonly<Partial<Record<NodeJS.Platform, string>>> = {
    win32: "tembra_node_binding.dll",
    darwin: "libtembra_node_binding.dylib",
    linux: "libtembra_node_binding.so",
  };
  const library = libraries[process.platform];
  if (library === undefined) {
    throw new Error("native binding build does not support this platform");
  }
  const output = path.join(
    sdk,
    "dist",
    "native",
    `tembra.${process.platform}-${process.arch}.node`,
  );
  mkdirSync(path.dirname(output), { recursive: true });
  copyFileSync(path.join(metadata.target_directory, profile, library), output);
} else if (kind === "wasm") {
  cargo([
    "build",
    "--locked",
    "-p",
    "tembra-browser-wasm",
    "--target",
    "wasm32-unknown-unknown",
    ...flags,
  ]);
  const output = path.join(sdk, "dist", "wasm");
  rmSync(output, { recursive: true, force: true });
  execFileSync(
    "wasm-bindgen",
    [
      "--target",
      "web",
      "--out-dir",
      output,
      "--out-name",
      "tembra_browser_wasm",
      path.join(
        metadata.target_directory,
        "wasm32-unknown-unknown",
        profile,
        "tembra_browser_wasm.wasm",
      ),
    ],
    { cwd: root, stdio: "inherit" },
  );
} else {
  throw new Error("expected native or wasm");
}
