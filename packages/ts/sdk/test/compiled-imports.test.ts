import { expect, test } from "bun:test";
import { createRequire } from "node:module";
import path from "node:path";
import { readFile } from "node:fs/promises";
import { createHost } from "../src/node.ts";
import { signature } from "../src/runtime/adapters.ts";

// The no-std pipeline component imports `arithmetic` synchronously and `delayed` through P3.
const consumer = process.env.TEMBRA_WASM_CONSUMER;
const binding = createRequire(import.meta.url)("@syntropika/tembra-engine-node");
const wit = path.resolve(import.meta.dir, "../../../../crates/executor-wasm/tests/wit/pipeline");
const pipeline = "example:pipeline/pipeline@1.0.0";
const arithmetic = "arithmetic::example:pipeline/arithmetic@1.0.0";
const requires = {
  "example:pipeline/arithmetic@1.0.0": { orb: "arithmetic", apiVersion: 1 },
  "example:pipeline/delayed@1.0.0": { orb: "delayed", apiVersion: 1 },
};

/** The consumer's typed reference; a trailing Context argument forwards caller authority. */
interface Pipeline {
  quadruple(value: number, context?: unknown): Promise<number>;
  octuple(value: number): Promise<number>;
  release(): void;
}

/** Package the compiled consumer or a closed source bundle for one pipeline world. */
async function artifact(world: string, name: string, source?: string): Promise<Uint8Array> {
  const contract = new binding.NodeContract(wit, world);
  const component = source === undefined ? new Uint8Array(await readFile(consumer!)) : undefined;
  return new Uint8Array(
    source === undefined
      ? contract.componentArtifact(component!, name, 1)
      : contract.sourceArtifact(source, name, 1),
  );
}

/** Read the WIT digest the consumer's installation requires from its arithmetic provider. */
function requiredSignature(bytes: Uint8Array): string {
  const reply = JSON.parse(
    new binding.NodeArtifact(bytes).request(
      JSON.stringify({
        kind: "install",
        name: "pipeline",
        apiVersion: 1,
        requires,
        grants: {},
        filesystem: [],
        network: [],
      }),
    ),
  );
  const requirement = reply.ok.plan.orb.requires.find(
    (item: { orb: string }) => item.orb === "arithmetic",
  );
  return requirement.signatures[arithmetic];
}

test.skipIf(!consumer)(
  "a trusted provider calling back into its suspended compiled caller is refused",
  async () => {
    const bytes = await artifact("consumer", "pipeline");
    let back: Pipeline | undefined;
    let refusal: unknown;
    const host = createHost();
    try {
      await host.install([
        {
          name: "arithmetic",
          apiVersion: 1,
          services: {
            [arithmetic]: {
              [signature]: requiredSignature(bytes),
              methods: {
                // Re-enter the compiled caller with the admitted dependency context.
                async double(_environment: unknown, context: unknown, value: number) {
                  try {
                    return await back!.quadruple(value, context);
                  } catch (error) {
                    refusal = error;
                    throw error;
                  }
                },
              },
            },
          },
        } as never,
        {
          artifact: await artifact(
            "delayed-provider",
            "delayed",
            "globalThis.__tembraOrb={services:{delayed:{methods:{async double(_e,_c,v){return v*2;}}}}};",
          ),
        },
        { artifact: bytes, requires },
      ]);
      const reference = (await host.get(`pipeline::${pipeline}`, "main")) as unknown as Pipeline;
      back = reference;
      const started = performance.now();
      await expect(reference.quadruple(1)).rejects.toThrow();
      // The kernel execution frame refused the reentry before it waited on the pipeline heap.
      expect(String((refusal as Error | undefined)?.message)).toContain("causal reentry");
      expect(performance.now() - started).toBeLessThan(2000);
      // The synchronous import could report that refusal only by trapping its importer.
      await expect(reference.octuple(1)).rejects.toThrow("component trap");
      reference.release();
    } finally {
      await host.close();
    }
  },
  60000,
);
