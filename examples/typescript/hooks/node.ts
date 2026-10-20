// Compose a portable hook through the same Rust Host used by ordinary facets.
import { createHost } from "@tembra/sdk/node";

import { normalizeName } from "./normalize-name.ts";

const host = await createHost({ name: "hooks-example" });
try {
  console.log(await normalizeName(host, "  Tembra  "));
} finally {
  await host.close();
}
