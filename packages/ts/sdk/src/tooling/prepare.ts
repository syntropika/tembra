import { createHash } from "node:crypto";
import path from "node:path";
import ts from "typescript";
import { resolver } from "./resolver.ts";
import { typecheck } from "./typecheck.ts";
import { Platform } from "./types.ts";
import { witTypes } from "./wit.ts";

export interface PrepareOptions {
  entry: string;
  wit: string;
  world?: string;
  root?: string;
}

export interface PreparedSource {
  readonly format: "tembra-interpreter-source-v1";
  readonly source: string;
  readonly sha256: string;
  readonly typesSha256: string;
  readonly contract: readonly number[];
  readonly world?: string;
}

/** Typecheck direct WIT imports and bundle the installed dependency closure without executing it. */
export async function prepareSource(options: PrepareOptions): Promise<PreparedSource> {
  const root = path.resolve(options.root ?? process.cwd());
  const entry = path.resolve(root, options.entry);
  const types = await witTypes(path.resolve(root, options.wit), options.world);
  typecheck(entry, root, types.files);
  const target = { entry, platform: Platform.Embedded, minify: false };
  const result = await Bun.build({
    entrypoints: ["tembra:entry"],
    root,
    format: "iife",
    target: "browser",
    splitting: false,
    plugins: [resolver(root, target, "tembra-wasm")],
  });
  if (!result.success) throw new AggregateError(result.logs, "Orb preparation failed");
  if (result.outputs.length !== 1 || result.outputs[0]?.kind !== "entry-point")
    throw new Error("Interpreter Orbs require one closed source bundle");
  const source = await result.outputs[0].text();
  const parsed = ts.createSourceFile("orb.js", source, ts.ScriptTarget.Latest, true);
  /** Refuse unresolved module operations before code reaches the interpreter. */
  function closed(node: ts.Node): void {
    if (
      ts.isImportDeclaration(node) ||
      ts.isExportDeclaration(node) ||
      (ts.isCallExpression(node) &&
        (node.expression.kind === ts.SyntaxKind.ImportKeyword ||
          (ts.isIdentifier(node.expression) && node.expression.text === "require")))
    )
      throw new Error("Orb bundle contains an unresolved module operation");
    ts.forEachChild(node, closed);
  }
  closed(parsed);
  return Object.freeze({
    format: "tembra-interpreter-source-v1",
    source,
    sha256: createHash("sha256").update(source).digest("hex"),
    typesSha256: types.sha256,
    contract: Object.freeze(Array.from(types.contract)),
    ...(options.world ? { world: options.world } : {}),
  });
}
