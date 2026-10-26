import { spawnSync } from "node:child_process";
import { copyFileSync, mkdirSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../../../", import.meta.url));
const destination = fileURLToPath(new URL("./dist/", import.meta.url));
const debug = process.argv.includes("--debug");
const features = process.argv.find((argument) => argument.startsWith("--features="))?.slice(11);
const args = ["build", "--locked", "-p", "tembra-node-binding"];
if (!debug) args.push("--release");
if (features) args.push("--features", features);

const result = spawnSync("cargo", args, { cwd: root, stdio: "inherit" });
if (result.error) throw result.error;
if (result.status !== 0) throw new Error(`cargo exited with ${result.status}`);

const library =
  process.platform === "win32"
    ? "tembra_node_binding.dll"
    : process.platform === "darwin"
      ? "libtembra_node_binding.dylib"
      : "libtembra_node_binding.so";
const target = path.resolve(root, process.env.CARGO_TARGET_DIR ?? "target");
mkdirSync(destination, { recursive: true });
copyFileSync(
  path.join(target, debug ? "debug" : "release", library),
  path.join(destination, `tembra-${process.platform}-${process.arch}.node`),
);
