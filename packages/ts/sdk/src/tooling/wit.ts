import { createHash } from "node:crypto";
import path from "node:path";
import ts from "typescript";
import type { NodeContract } from "@syntropika/tembra-engine-node";
import { query } from "../interpreter/types.ts";

export interface WitTypes {
  readonly files: ReadonlyMap<string, string>;
  readonly sha256: string;
  readonly contract: NodeContract;
  /** jco declarations of imported standard interfaces, keyed by WIT import name. */
  readonly paths: Readonly<Record<string, string[]>>;
}

/** Generate official WIT declarations in memory and expose a type-only .wit module. */
export async function witTypes(file: string, world?: string): Promise<WitTypes> {
  const { NodeContract } = await import("@syntropika/tembra-engine-node");
  const { $init, generateTypes } = await import("@bytecodealliance/jco-transpile/component");
  const absolute = path.resolve(file);
  if (!absolute.endsWith(".wit")) throw new Error("Direct WIT imports require a .wit entry file");
  const contract = new NodeContract(path.dirname(absolute), world);
  await $init;
  const generated = generateTypes("contract", {
    wit: { tag: "binary", val: contract.wit() },
    ...(world ? { world } : {}),
  });
  const entries = [...generated]
    .map(([name, bytes]): [string, Uint8Array] => {
      if (!name.endsWith(".d.ts")) return [name, bytes];
      const text = new TextDecoder().decode(bytes);
      const source = ts.createSourceFile(name, text, ts.ScriptTarget.ESNext, true);
      // WIT resources always have a canonical drop. The pinned jco declarations omit it;
      // declaration merging exposes the runtime's Symbol.dispose without copying WIT types.
      const resources = source.statements.filter(
        (statement): statement is ts.ClassDeclaration =>
          ts.isClassDeclaration(statement) &&
          statement.modifiers?.some((modifier) => modifier.kind === ts.SyntaxKind.ExportKeyword) ===
            true,
      );
      const disposable = resources.map(
        (resource) => `export interface ${resource.name!.text} extends Disposable {}`,
      );
      return [
        name,
        disposable.length
          ? new TextEncoder().encode(text + "\n" + disposable.join("\n") + "\n")
          : bytes,
      ];
    })
    .sort(([left], [right]) => left.localeCompare(right));
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
  // Standard imports retain jco's WIT shapes and the canonical resource-drop declaration.
  const paths: Record<string, string[]> = {};
  const description = query<{ imports: { contract?: string }[] }>(contract, { kind: "describe" });
  for (const imported of description.imports) {
    if (!imported.contract?.startsWith("wasi:")) continue;
    const [, namespace, name] = /^(\w+):(.+)@/.exec(imported.contract)!;
    const declaration = path.join(
      directory,
      "interfaces",
      `${namespace}-${name!.replace("/", "-")}.d.ts`,
    );
    if (files.has(declaration)) paths[imported.contract] = [declaration];
  }
  return { files, sha256, contract, paths };
}
