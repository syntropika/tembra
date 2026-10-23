import { createHost } from "@tembra/sdk/browser";

import { bindingWorkload } from "./workload.ts";

export async function browserAcceptance() {
  const parameters = new URLSearchParams(location.search);
  const sampleEvery = parameters.get("sample_every");
  const beforeResponse = await fetch("/benchmark-metadata.json");
  const before: unknown = await beforeResponse.json();
  const result = await bindingWorkload(createHost, {
    samples: Number(parameters.get("samples") ?? 5),
    warmup: Number(parameters.get("warmup") ?? 2),
    operations: Number(parameters.get("operations") ?? 128),
    batch: Number(parameters.get("batch") ?? 8),
    concurrency: Number(parameters.get("concurrency") ?? 16),
    ...(sampleEvery === null
      ? {}
      : {
          observability: {
            capacity: Number(parameters.get("capacity") ?? 131_072),
            sample_every: Number(sampleEvery),
          },
        }),
  });
  const afterResponse = await fetch("/benchmark-metadata.json");
  const after: unknown = await afterResponse.json();
  if (JSON.stringify(before) !== JSON.stringify(after)) {
    throw new Error("benchmark source or WASM changed during capture");
  }
  return { binding: "browser", user_agent: navigator.userAgent, metadata: before, ...result };
}
