import { createHost } from "@tembra/sdk/node";

import { benchmarkMetadata } from "../../../performance/metadata.ts";
import { bindingWorkload } from "./workload.ts";

function integer(name: string, fallback: number): number {
  const value = process.env[name];
  return value === undefined ? fallback : Number(value);
}
const sampleEvery = process.env.TEMBRA_OBSERVABILITY_SAMPLE_EVERY;
const binaryName = `packages/sdk/dist/native/tembra.${process.platform}-${process.arch}.node`;
const binary = new URL(`../../../../${binaryName}`, import.meta.url);
const before = benchmarkMetadata([binaryName, "packages/sdk/dist/src"]);
const result = await bindingWorkload(async (options) => await createHost({ ...options, binary }), {
  samples: integer("TEMBRA_BENCH_SAMPLES", 5),
  warmup: integer("TEMBRA_BENCH_WARMUP", 2),
  operations: integer("TEMBRA_BENCH_OPERATIONS", 128),
  batch: integer("TEMBRA_BENCH_BATCH", 8),
  concurrency: integer("TEMBRA_BENCH_CONCURRENCY", 16),
  ...(sampleEvery === undefined
    ? {}
    : {
        observability: {
          capacity: integer("TEMBRA_OBSERVABILITY_CAPACITY", 131_072),
          sample_every: Number(sampleEvery),
        },
      }),
});
const after = benchmarkMetadata([binaryName, "packages/sdk/dist/src"]);
if (JSON.stringify(before) !== JSON.stringify(after)) {
  throw new Error("benchmark source or executable changed during capture");
}
// Results are emitted only after all timing and managed cleanup have completed.
console.log(
  JSON.stringify({
    binding: "node",
    node: process.version,
    platform: process.platform,
    architecture: process.arch,
    metadata: before,
    ...result,
  }),
);
