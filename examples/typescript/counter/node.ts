// Start here: a typed contract, a callback facet and managed state in Node.
import { createHost } from "@tembra/sdk/node";

import { Counter } from "./contract.ts";
import { counterFacet } from "./facet.ts";

const host = await createHost({ name: "counter-example" });
try {
  await host.install(counterFacet);
  const counter = await host.get(Counter, {});
  const unsubscribe = counter.count.subscribe((count) => {
    console.log(`count: ${count}`);
  });
  try {
    console.log(`increment: ${await counter.increment()}`);
  } finally {
    unsubscribe();
    counter.release();
  }
} finally {
  await host.close();
}
