import { execFileSync } from "node:child_process";
import { existsSync, readdirSync, rmSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const sdk = fileURLToPath(new URL("../", import.meta.url));
const output = path.join(sdk, "dist");
// Platform bindings have independent release/debug builds; every other output belongs to tsc.
if (existsSync(output)) {
  for (const entry of readdirSync(output)) {
    if (entry !== "native" && entry !== "wasm") {
      rmSync(path.join(output, entry), { recursive: true, force: true });
    }
  }
}
execFileSync(process.execPath, ["--bun", "x", "--no-install", "tsc", "-p", "tsconfig.build.json"], {
  cwd: sdk,
  stdio: "inherit",
});
