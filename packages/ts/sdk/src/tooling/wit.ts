import { createHash } from "node:crypto";
import path from "node:path";

export interface WitTypes {
  readonly files: ReadonlyMap<string, string>;
  readonly sha256: string;
  readonly contract: Uint8Array;
}

/** Generate official WIT declarations in memory and expose a type-only .wit module. */
export async function witTypes(file: string, world?: string): Promise<WitTypes> {
  const { compileContract } = await import("@syntropika/tembra-engine-node");
  const { $init, generateTypes } = await import("@bytecodealliance/jco-transpile/component");
  const absolute = path.resolve(file);
  if (!absolute.endsWith(".wit")) throw new Error("Direct WIT imports require a .wit entry file");
  const binary = compileContract(path.dirname(absolute), world);
  await $init;
  const generated = generateTypes("contract", {
    wit: { tag: "binary", val: binary },
    ...(world ? { world } : {}),
  });
  const entries = [...generated].sort(([left], [right]) => left.localeCompare(right));
  const digest = createHash("sha256");
  for (const [name, bytes] of entries)
    digest.update(JSON.stringify([name, bytes.length])).update(bytes);
  const sha256 = digest.digest("hex");
  const directory = path.join(path.dirname(absolute), ".tembra", "types", sha256);
  const files = new Map(
    entries.map(([name, bytes]) => [path.join(directory, name), new TextDecoder().decode(bytes)]),
  );
  const alias = absolute.slice(0, -4) + ".d.wit.ts";
  const target =
    "./" +
    path
      .relative(path.dirname(alias), path.join(directory, "contract.js"))
      .split(path.sep)
      .join("/");
  files.set(alias, `export * from ${JSON.stringify(target)};\n`);
  return { files, sha256, contract: binary };
}
