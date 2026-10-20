import { test } from "node:test";

import { createHost } from "@tembra/sdk/node";

import { detachedHookOwnership } from "../../src/detached-hooks.ts";

await test(
  "detached portable hooks retain their originating invocation through release, reload and close",
  { timeout: 10_000 },
  async () => {
    await detachedHookOwnership(createHost);
  },
);
