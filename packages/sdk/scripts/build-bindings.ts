import { spawnSync } from "node:child_process";
import { copyFileSync, mkdirSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../../../", import.meta.url));
const destination = fileURLToPath(new URL("../dist/", import.meta.url));
const mode = process.argv[2] ?? "native";
const debug = process.argv.includes("--debug");
const features = process.argv.find((argument) => argument.startsWith("--features="))?.slice(11);

function run(command: string, arguments_: string[]) {
  const result = spawnSync(command, arguments_, { cwd: root, stdio: "inherit" });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`${command} exited with ${result.status}`);
}

const profile = debug ? "debug" : "release";
const arguments_ = ["build", "-p", mode === "wasm" ? "tembra-browser-wasm" : "tembra-node-binding"];
if (!debug) arguments_.push("--release");
if (features) arguments_.push("--features", features);

if (mode === "wasm") {
  arguments_.push("--target", "wasm32-unknown-unknown");
  run("cargo", arguments_);
  mkdirSync(path.join(destination, "wasm"), { recursive: true });
  run("wasm-bindgen", [
    "--target",
    "web",
    "--out-dir",
    path.join(destination, "wasm"),
    "--out-name",
    "tembra",
    path.join(root, "target", "wasm32-unknown-unknown", profile, "tembra_browser_wasm.wasm"),
  ]);
} else if (mode === "native") {
  run("cargo", arguments_);
  mkdirSync(path.join(destination, "native"), { recursive: true });
  const library =
    process.platform === "win32"
      ? "tembra_node_binding.dll"
      : process.platform === "darwin"
        ? "libtembra_node_binding.dylib"
        : "libtembra_node_binding.so";
  copyFileSync(
    path.join(root, "target", profile, library),
    path.join(destination, "native", "tembra.node"),
  );
} else throw new Error("Choose native or wasm");
