import { spawnSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { readFileSync, writeFileSync } from "node:fs";
import ts from "typescript";

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

// Build the prepared-contract validator separately from the lifecycle kernel.
const contracts = [...args];
contracts[contracts.indexOf("tembra-browser-wasm")] = "tembra-contracts-wasm";
const featureIndex = contracts.indexOf("--features");
if (featureIndex >= 0) contracts.splice(featureIndex, 2);
run("cargo", contracts);
run("wasm-bindgen", [
  "--target",
  "web",
  "--out-dir",
  destination,
  "--out-name",
  "contracts",
  path.join(
    target,
    "wasm32-unknown-unknown",
    debug ? "debug" : "release",
    "tembra_contracts_wasm.wasm",
  ),
]);

// Keep the asset locator beside the generated loaders and its interpreter directory.
const assets = await Bun.build({
  entrypoints: [fileURLToPath(new URL("./assets.ts", import.meta.url))],
  outdir: destination,
  naming: "assets.js",
  target: "browser",
  format: "esm",
});
if (!assets.success) throw new AggregateError(assets.logs, "Engine asset locator build failed");
const declarations = ts.transpileDeclaration(
  readFileSync(new URL("./assets.ts", import.meta.url), "utf8"),
  { fileName: "assets.ts" },
);
if (declarations.diagnostics?.length)
  throw new Error("Engine asset locator declaration generation failed");
writeFileSync(path.join(destination, "assets.d.ts"), declarations.outputText);
