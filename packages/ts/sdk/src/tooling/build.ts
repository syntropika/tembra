import { createHash } from "node:crypto";
import { mkdir, readFile, rename, writeFile } from "node:fs/promises";
import path from "node:path";
import ts from "typescript";

import { NodeContract } from "@syntropika/tembra-engine-node";
import { fileURLToPath } from "node:url";
import { describe, witTypes, type DescribedFunction, type Description } from "./wit.ts";
import { resolver } from "./resolver.ts";
import { typecheck } from "./typecheck.ts";
import type { Artifact, BuildConfig, WitImports } from "./types.ts";

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
      const registration = identity(program!, entry);
      const generated = sourceEntry(describe(contract), entry, registration.hooks);
      const result = await Bun.build({
        entrypoints: ["tembra:entry"],
        root,
        format: "esm",
        target: "browser",
        splitting: false,
        minify: target.minify ?? true,
        ...(target.define ? { define: { ...target.define } } : {}),
        plugins: [resolver(root, generated.source, generated.imports)],
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
      closed(name, source, generated.imports);
      bytes = contract.sourceArtifact(
        source,
        await runtime(),
        registration.name,
        registration.apiVersion,
        registration.hooks,
      );
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
function identity(
  program: ts.Program,
  entry: string,
): { name: string; apiVersion: number; hooks: string[] } {
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
  // Provided hooks become typed self-imports of the prepared world, so their keys are literal.
  const hooks = property("hooks");
  const aliases = hooks
    ? checker.getPropertiesOfType(checker.getNonNullableType(hooks)).map((symbol) => symbol.name)
    : [];
  if (hooks && checker.getIndexInfosOfType(checker.getNonNullableType(hooks)).length)
    throw new Error("Wasm source requires literal hook names");
  return { name: name.value, apiVersion: version.value, hooks: aliases.sort() };
}

/** The private interfaces of the component runtime the generated entry uses. */
const HOST = "tembra:orb/host@0.1.0";

/**
 * Read the prepared QuickJS runtime dylib shipped with the Node engine. `TEMBRA_QJS_RUNTIME`
 * names another runtime file for engine development only.
 */
async function runtime(): Promise<Uint8Array> {
  const override = process.env.TEMBRA_QJS_RUNTIME;
  const file =
    override ?? fileURLToPath(import.meta.resolve("@syntropika/tembra-engine-node/runtime.wasm"));
  return new Uint8Array(await readFile(file));
}

/** lowerCamelCase as componentize-qjs and the contract spell WIT names. */
function camel(name: string): string {
  return name.replace(/-([a-z0-9])/g, (_match, letter: string) => letter.toUpperCase());
}

/** The interface name componentize-qjs looks up for an exported interface. */
function exportAlias(binding: string): string {
  return camel(binding.split("/").at(-1)!.split("@")[0]!);
}

/** Generated entry module text and the WIT imports it may keep external. */
export interface SourceEntry {
  readonly source: string;
  readonly imports: WitImports;
}

/**
 * Generate the closed entry module: the author definition, the private shim and one typed
 * wrapper per exported public interface, with flags taken from the WIT, never from the author.
 */
export function sourceEntry(
  description: Description,
  entry: string,
  hooks: readonly string[],
  shim = shimModuleSync(),
): SourceEntry {
  const kinds = new Map(description.types.map((type) => [type.id, type.definition.kind]));
  const resolve = (type: unknown): unknown => {
    for (let depth = 0; typeof type === "number" && depth < 64; depth++) {
      const kind = kinds.get(type);
      if (!kind || typeof kind !== "object" || !("type" in kind)) return kind;
      type = (kind as { type: unknown }).type;
    }
    return type;
  };
  const isResult = (type: unknown) => {
    const kind = resolve(type);
    return typeof kind === "object" && kind !== null && "result" in kind;
  };
  const isStream = (type: unknown) => {
    const kind = resolve(type);
    return typeof kind === "object" && kind !== null && "stream" in kind;
  };
  const flags = (fn: DescribedFunction) => {
    if (typeof fn.kind !== "string")
      throw new Error(`${fn.name}: exported WIT resources are not supported by source Orbs yet`);
    const asynchronous = fn.kind === "async-freestanding";
    return (
      (asynchronous ? "a" : "") +
      (fn.result !== undefined && isResult(fn.result) ? "r" : "") +
      (asynchronous && fn.result !== undefined && isStream(fn.result) ? "s" : "")
    );
  };
  // The shim evaluates first: author modules then see its module-evaluation globals.
  const lines = [
    `import { start } from ${JSON.stringify(shim)};`,
    `import definition from ${JSON.stringify(entry)};`,
    `import host from ${JSON.stringify(HOST)};`,
  ];
  const direct = new Set<string>();
  const internal = new Set<string>([HOST]);
  const dependencies: string[] = [];
  for (const imported of description.imports) {
    if (!imported.contract || imported.type_only !== false) continue;
    if (imported.binding.startsWith("wasi:")) {
      direct.add(imported.binding);
      continue;
    }
    if (imported.binding.startsWith("tembra:orb/"))
      throw new Error(`${imported.binding} is private to the Tembra runtime`);
    const local = `dependency${dependencies.length}`;
    lines.push(`import ${local} from ${JSON.stringify(imported.binding)};`);
    dependencies.push(`${JSON.stringify(imported.binding)}: ${local}`);
    internal.add(imported.binding);
  }
  const exported: string[] = [];
  const aliases = new Map<string, string>();
  const hookBindings: string[] = [];
  for (const item of description.exports) {
    if (item.function)
      throw new Error(`${item.binding}: source Orbs export interfaces, not world functions`);
    if (!item.contract || item.type_only !== false || !item.functions) continue;
    const alias = exportAlias(item.binding);
    if (alias === "lifecycle")
      throw new Error(`${item.binding}: the export name lifecycle is reserved`);
    if (aliases.has(alias))
      throw new Error(`${item.binding} and ${aliases.get(alias)} share the export name ${alias}`);
    aliases.set(alias, item.binding);
    const methods = Object.values(item.functions).map(
      (fn) => `${JSON.stringify(camel(fn.name))}: ${JSON.stringify(flags(fn))}`,
    );
    exported.push(
      `export const ${alias} = orb.exported(${JSON.stringify(item.binding)}, ${JSON.stringify(alias)}, { ${methods.join(", ")} });`,
    );
    if (hooks.includes(alias)) {
      const functions = Object.values(item.functions);
      if (functions.length !== 1)
        throw new Error(`${item.binding}: a hook interface declares exactly one function`);
      const local = `hook${hookBindings.length}`;
      lines.push(`import ${local} from ${JSON.stringify(item.binding)};`);
      hookBindings.push(
        `${JSON.stringify(alias)}: { module: ${local}, method: ${JSON.stringify(camel(functions[0]!.name))} }`,
      );
      internal.add(item.binding);
    }
  }
  for (const alias of hooks)
    if (!aliases.has(alias)) throw new Error(`Hook ${alias} is not an exported WIT interface`);
  lines.push(
    `const orb = start(host, definition, { dependencies: { ${dependencies.join(", ")} }, hooks: { ${hookBindings.join(", ")} } });`,
    "export const lifecycle = orb.lifecycle;",
    ...exported,
  );
  return { source: lines.join("\n") + "\n", imports: { direct, entry: internal } };
}

/** Locate the private guest shim in source or installed output. */
function shimModuleSync(): string {
  const source = fileURLToPath(new URL("../guest/shim.ts", import.meta.url));
  return import.meta.url.endsWith(".ts")
    ? source
    : fileURLToPath(new URL("../guest/shim.js", import.meta.url));
}

/** Refuse any module operation other than the static WIT imports the runtime resolves. */
function closed(name: string, source: string, imports: WitImports): void {
  const parsed = ts.createSourceFile(
    "bundle.js",
    source,
    ts.ScriptTarget.Latest,
    true,
    ts.ScriptKind.JS,
  );
  function verify(node: ts.Node): void {
    if (ts.isImportDeclaration(node)) {
      const specifier = (node.moduleSpecifier as ts.StringLiteral).text;
      if (!imports.direct.has(specifier) && !imports.entry.has(specifier))
        throw new Error(`Build ${name} contains an unresolved import ${specifier}`);
    }
    if (ts.isExportDeclaration(node) && node.moduleSpecifier)
      throw new Error(`Build ${name} contains an unresolved re-export`);
    if (
      ts.isCallExpression(node) &&
      (node.expression.kind === ts.SyntaxKind.ImportKeyword ||
        (ts.isIdentifier(node.expression) && node.expression.text === "require"))
    )
      throw new Error(`Build ${name} contains an unresolved import`);
    ts.forEachChild(node, verify);
  }
  verify(parsed);
}
