import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const workspace = fileURLToPath(new URL("../../../", import.meta.url));
for (const arguments_ of [
  ["build", "--locked", "-p", "tembra-testing", "--bin", "tembra-testing-emit-contract"],
  ["build", "--locked", "-p", "tembra-codegen"],
]) {
  execFileSync("cargo", arguments_, { cwd: workspace, stdio: "inherit" });
}
