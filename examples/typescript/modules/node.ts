// Load a module, then reload it while retaining the same reference and state.
import { readFile } from "node:fs/promises";

import type { LoadableFacet } from "@tembra/sdk";
import { createHost } from "@tembra/sdk/node";

import { Counter } from "../counter/contract.ts";

const source = process.argv[2] ?? new URL("../counter/facet.ts", import.meta.url);
async function input(): Promise<LoadableFacet> {
  if (typeof source === "string" && source.endsWith(".json")) {
    // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- Host.load validates the parsed artifact before evaluation.
    return JSON.parse(await readFile(source, "utf-8")) as LoadableFacet;
  }
  return source;
}

const host = await createHost({ name: "module-example" });
try {
  await host.load(await input());
  const counter = await host.get(Counter, {});
  try {
    console.log(`before reload: ${await counter.increment()}`);
    await host.reload([await input()]);
    console.log(`after reload, same reference: ${await counter.increment()}`);
  } finally {
    counter.release();
  }
} finally {
  await host.close();
}
