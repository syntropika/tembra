import { appendFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { validate } from "./release/metadata.ts";
import { pack, publishNpm } from "./release/npm.ts";
import { publishCrates } from "./release/crates.ts";

const root = fileURLToPath(new URL("../", import.meta.url));
const [command, tag, destination] = process.argv.slice(2);
if (!tag)
  throw new Error(
    "Usage: bun scripts/release.ts <validate|pack|publish-npm|publish-crates> <tag> [artifact-directory]",
  );
const release = validate(root, tag);
if (command === "validate") {
  if (process.env.GITHUB_OUTPUT)
    appendFileSync(
      process.env.GITHUB_OUTPUT,
      `version=${release.version}\nchannel=${release.channel}\n`,
    );
  console.log(`Release ${release.version} (${release.channel})`);
} else if (command === "pack" && destination) {
  pack(root, path.resolve(destination), release.version);
} else if (command === "publish-npm" && destination) {
  await publishNpm(path.resolve(destination), release.channel);
} else if (command === "publish-crates" && destination) {
  await publishCrates(root, path.resolve(destination), release.version);
} else throw new Error("Unknown release command or missing artifact directory");
