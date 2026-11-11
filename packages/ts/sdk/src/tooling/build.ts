import { createHash } from "node:crypto";
import { mkdir, rename, writeFile } from "node:fs/promises";
import path from "node:path";
import ts from "typescript";

import { NodeContract } from "@syntropika/tembra-engine-node";
import { query } from "../interpreter/types.ts";
import { witTypes } from "./wit.ts";
import { resolver } from "./resolver.ts";
import { typecheck } from "./typecheck.ts";
import type { Artifact, BuildConfig } from "./types.ts";

/** Resolve dependencies and WIT types, then emit one closed Wasm artifact per Orb. */
export async function build(config: BuildConfig): Promise<readonly Artifact[]> {
  if (typeof Bun === "undefined") throw new Error("Tembra build requires Bun");
  const root = path.resolve(config.root ?? process.cwd());
  const outDir = path.resolve(root, config.outDir ?? "dist/tembra");
  const artifacts: Artifact[] = [];
  const worlds = new Map<string, string | undefined>();
  for (const target of Object.values(config.builds)) {
    if (target.entry.endsWith(".wasm")) continue;
    const file = path.resolve(root, target.wit);
    if (worlds.has(file) && worlds.get(file) !== target.world)
      throw new Error(`Source builds for ${target.wit} must select the same WIT world`);
    worlds.set(file, target.world);
  }
  for (const [name, target] of Object.entries(config.builds)) {
    if (!/^[\w-]+$/.test(name)) throw new Error("Invalid build name");
    const entry = path.resolve(root, target.entry);
    const wit = path.resolve(root, target.wit);
    const compiled = entry.endsWith(".wasm");
    const types = compiled ? undefined : await witTypes(wit, target.world);
    if (types) {
      for (const [file, contents] of types.files) {
        await mkdir(path.dirname(file), { recursive: true });
        await writeFile(file, contents);
      }
    }
    let bytes: Uint8Array;
    const contract = types?.contract ?? new NodeContract(path.dirname(wit), target.world);
    if (compiled) {
      if (!target.apiVersion)
        throw new Error(`Build ${name} requires apiVersion for a compiled Orb`);
      bytes = contract.componentArtifact(
        await Bun.file(entry).bytes(),
        name,
        target.apiVersion,
        target.contributes,
      );
    } else {
      if (target.apiVersion !== undefined)
        throw new Error(`Build ${name} declares apiVersion in its source Orb`);
      if (target.contributes !== undefined)
        throw new Error(`Build ${name} declares contributions in its source Orb`);
      const program = typecheck(entry, root, types!.files, ["tembra-wasm"], types!.paths);
      const description = query<{ imports: { contract?: string; type_only?: boolean }[] }>(
        contract,
        { kind: "describe" },
      );
      const imports = new Set(
        description.imports.flatMap((item) =>
          item.type_only === false && item.contract ? [item.contract] : [],
        ),
      );
      const result = await Bun.build({
        entrypoints: ["tembra:entry"],
        root,
        format: "iife",
        target: "browser",
        splitting: false,
        minify: target.minify ?? true,
        ...(target.define ? { define: { ...target.define } } : {}),
        plugins: [resolver(root, target, imports)],
      }).catch((error: unknown) => {
        if (error instanceof AggregateError)
          throw new AggregateError(
            error.errors,
            `Build ${name} failed: ${error.errors.map(String).join("\n")}`,
          );
        throw error;
      });
      if (!result.success) throw new AggregateError(result.logs, `Build ${name} failed`);
      if (result.outputs.length !== 1 || result.outputs[0]?.kind !== "entry-point")
        throw new Error(`Build ${name} requires exactly one closed bundle`);
      const source = await result.outputs[0]!.text();
      const parsed = ts.createSourceFile(
        "bundle.js",
        source,
        ts.ScriptTarget.Latest,
        true,
        ts.ScriptKind.JS,
      );
      /** Reject module operations that cannot resolve in the selected runtime. */
      function verify(node: ts.Node): void {
        let specifier: ts.Expression | undefined;
        if (ts.isImportDeclaration(node) || ts.isExportDeclaration(node))
          specifier = node.moduleSpecifier;
        if (
          ts.isCallExpression(node) &&
          (node.expression.kind === ts.SyntaxKind.ImportKeyword ||
            (ts.isIdentifier(node.expression) && node.expression.text === "require"))
        )
          specifier = node.arguments[0];
        if (specifier) throw new Error(`Build ${name} contains an unresolved import`);
        ts.forEachChild(node, verify);
      }
      verify(parsed);
      const registration = identity(program!, entry);
      bytes = contract.sourceArtifact(source, registration.name, registration.apiVersion);
    }
    const hash = createHash("sha256").update(bytes).digest("hex");
    const directory = path.join(outDir, name);
    const destination = path.join(directory, `orb.${hash}.wasm`);
    await mkdir(directory, { recursive: true });
    const temporary = `${destination}.${process.pid}.tmp`;
    await writeFile(temporary, bytes);
    await rename(temporary, destination);
    artifacts.push({
      name,
      path: destination,
      sha256: hash,
      bytes: bytes.length,
    });
  }
  return artifacts;
}

/** Read literal registration metadata from types without evaluating the author module. */
function identity(program: ts.Program, entry: string): { name: string; apiVersion: number } {
  const checker = program.getTypeChecker();
  const file = program.getSourceFile(entry)!;
  const module = checker.getSymbolAtLocation(file);
  const exported =
    module && checker.getExportsOfModule(module).find((symbol) => symbol.name === "default");
  if (!exported) throw new Error("Wasm source must export a default orb");
  const definition = checker.getTypeOfSymbolAtLocation(exported, file);
  const property = (name: string) => {
    const symbol = definition.getProperty(name);
    return symbol && checker.getTypeOfSymbolAtLocation(symbol, file);
  };
  const name = property("name");
  const version = property("apiVersion");
  if (
    !name?.isStringLiteral() ||
    !version?.isNumberLiteral() ||
    !name.value ||
    !Number.isInteger(version.value) ||
    version.value <= 0
  )
    throw new Error("Wasm source requires literal name and positive apiVersion metadata");
  return { name: name.value, apiVersion: version.value };
}
