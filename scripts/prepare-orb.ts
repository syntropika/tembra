import { parseArgs } from "node:util";
import { prepareComponent, prepareSource } from "../packages/ts/sdk/src/tooling/index.ts";

const { values } = parseArgs({
  args: Bun.argv.slice(2),
  options: {
    entry: { type: "string" },
    component: { type: "string" },
    wit: { type: "string" },
    world: { type: "string" },
    out: { type: "string" },
  },
  strict: true,
});
if (!values.entry === !values.component || !values.wit || !values.out)
  throw new Error(
    "Usage: bun scripts/prepare-orb.ts (--entry orb.ts | --component orb.wasm) --wit contract.wit [--world name] --out prepared.json",
  );
const contract = { wit: values.wit, ...(values.world ? { world: values.world } : {}) };
const prepared = values.entry
  ? await prepareSource({ ...contract, entry: values.entry })
  : await prepareComponent({ ...contract, component: values.component! });
await Bun.write(values.out, JSON.stringify(prepared) + "\n");
console.log(
  JSON.stringify({
    format: prepared.format,
    sha256: prepared.sha256,
    bytes:
      prepared.format === "tembra-component-v1"
        ? prepared.bytes.length
        : Buffer.byteLength(prepared.source),
    contractBytes: prepared.contract.length,
  }),
);
