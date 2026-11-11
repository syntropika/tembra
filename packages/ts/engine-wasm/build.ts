import { spawnSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { copyFileSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";

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
  const result = spawnSync(command, arguments_, {
    cwd: root,
    stdio: "inherit",
  });
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

// Keep compilation lazy at runtime and ship a browser-only trusted implementation. Removing
// Node branches here lets applications bundle it without adding Node polyfills.
const compilerEntry = fileURLToPath(new URL("./compiler.ts", import.meta.url));
const compilerVendor = Bun.resolveSync(
  "@bytecodealliance/jco-transpile/component",
  import.meta.dir,
);
const compilerAssets = new Set<string>();
const compiler = await Bun.build({
  entrypoints: [compilerEntry],
  outdir: destination,
  naming: "compiler.js",
  target: "browser",
  format: "esm",
  define: { process: "undefined" },
  plugins: [
    {
      name: "component-compiler-assets",
      setup(builder) {
        builder.onLoad({ filter: /js-component-bindgen-component\.js$/ }, (args) => {
          const contents = readFileSync(args.path, "utf8").replace(
            /new URL\('\.\/(js-component-bindgen-component\.core[0-9]*\.wasm)', import\.meta\.url\)/g,
            (_match, name: string) => {
              compilerAssets.add(name);
              return `new URL("./compiler/${name}", import.meta.url)`;
            },
          );
          return { contents, loader: "js" };
        });
      },
    },
  ],
});
if (!compiler.success)
  throw new AggregateError(compiler.logs, "Browser component compiler build failed");
if (compilerAssets.size !== 2) throw new Error("Pinned component compiler assets changed");
mkdirSync(path.join(destination, "compiler"), { recursive: true });
for (const name of compilerAssets)
  copyFileSync(
    path.join(path.dirname(compilerVendor), name),
    path.join(destination, "compiler", name),
  );
writeFileSync(path.join(destination, "compiler.d.ts"), readFileSync(compilerEntry, "utf8"));
