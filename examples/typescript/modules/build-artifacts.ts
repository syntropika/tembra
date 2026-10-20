// Publish a checked closed artifact through the SDK's programmatic build API.
import { fileURLToPath } from "node:url";

import { build, loadConfig } from "@tembra/sdk/tooling";

const config = await loadConfig(fileURLToPath(new URL("tembra.config.ts", import.meta.url)));
const result = await build(config);
for (const output of result.artifacts) {
  console.log(output.artifactPath);
}
