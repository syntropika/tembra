import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";
import path from "node:path";
import { NodeContract } from "@syntropika/tembra-engine-node";
import { witTypes } from "./wit.ts";
import { query, type PreparedComponent } from "../interpreter/types.ts";

/** Close a compiled component with its canonical contract, checking full exported signatures. */
export async function prepareComponent(options: {
  component: string;
  wit: string;
  world?: string;
  root?: string;
}): Promise<PreparedComponent> {
  const root = path.resolve(options.root ?? process.cwd());
  const bytes = await readFile(path.resolve(root, options.component));
  const types = await witTypes(path.resolve(root, options.wit), options.world);
  const contract = new NodeContract(types.contract, options.world);
  query(contract, { kind: "component", bytes: Array.from(bytes) });
  return Object.freeze({
    format: "tembra-component-v1",
    bytes: Object.freeze(Array.from(bytes)),
    sha256: createHash("sha256").update(bytes).digest("hex"),
    contract: Object.freeze(Array.from(types.contract)),
    ...(options.world ? { world: options.world } : {}),
  });
}
