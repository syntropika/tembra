import test from "node:test";

import { createHost } from "@tembra/sdk/node";

import { observationAcceptance } from "../../src/observability.ts";

await test(
  "real Node observations, bounded loss, State and final managed cleanup",
  { timeout: 10_000 },
  async () => {
    await observationAcceptance(createHost);
  },
);
