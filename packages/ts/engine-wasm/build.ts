import { spawnSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../../../", import.meta.url));
const destination = fileURLToPath(new URL("./dist/", import.meta.url));
const debug = process.argv.includes("--debug");
const features = process.argv.find((argument) => argument.startsWith("--features="))?.slice(11);
const args = [
  "build",
  "--locked",
  "-p",
  "tembra-browser-wasm",
  "--target",
  "wasm32-unknown-unknown",
];
if (!debug) args.push("--release");
if (features) args.push("--features", features);

/** Stop packaging when a compiler or binding-generation step fails. */
function run(command: string, arguments_: string[]): void {
  const result = spawnSync(command, arguments_, { cwd: root, stdio: "inherit" });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`${command} exited with ${result.status}`);
}

run("cargo", args);
const target = path.resolve(root, process.env.CARGO_TARGET_DIR ?? "target");
run("wasm-bindgen", [
  "--target",
  "web",
  "--out-dir",
  destination,
  "--out-name",
  "tembra",
  path.join(
    target,
    "wasm32-unknown-unknown",
    debug ? "debug" : "release",
    "tembra_browser_wasm.wasm",
  ),
]);
