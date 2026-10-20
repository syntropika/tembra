// The browser uses the same contract and facet as the Node counter example.
import { createHost } from "@tembra/sdk/browser";

import { Counter } from "../counter/contract.ts";
import { counterFacet } from "../counter/facet.ts";

const output = document.querySelector("output");
if (output === null) {
  throw new Error("example requires an output element");
}
const host = await createHost({ name: "browser-counter" });
try {
  // Vite serves this TypeScript module as JavaScript for the browser's native loader.
  const module = new URL("../counter/facet.ts", import.meta.url);
  await (new URL(location.href).searchParams.has("module")
    ? host.load(module)
    : host.install(counterFacet));
  const counter = await host.get(Counter, {});
  const unsubscribe = counter.count.subscribe((count) => {
    output.textContent += `count: ${count}\n`;
  });
  try {
    output.textContent += `increment: ${await counter.increment()}\n`;
  } finally {
    unsubscribe();
    counter.release();
  }
} finally {
  await host.close();
}
