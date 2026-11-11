import { createHash } from "node:crypto";
import path from "node:path";
import ts from "typescript";
import type { NodeContract } from "@syntropika/tembra-engine-node";
import { query } from "../interpreter/types.ts";

export interface WitTypes {
  readonly files: ReadonlyMap<string, string>;
  readonly sha256: string;
  readonly contract: NodeContract;
  readonly imports: ReadonlyMap<string, string>;
}

/** Generate official WIT declarations in memory and expose a type-only .wit module. */
export async function witTypes(
  file: string,
  world?: string,
  root = path.dirname(path.resolve(file)),
): Promise<WitTypes> {
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
          ? new TextEncoder().encode(
              '/// <reference lib="esnext.disposable" />\n' +
                text +
                "\n" +
                disposable.join("\n") +
                "\n",
            )
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
  // Share one physical ambient module across contracts in the same editor project.
  const references: string[] = [];
  const imports = new Map<string, string>();
  const generatedFiles = new Map(
    entries.map(([name, bytes]) => [name, new TextDecoder().decode(bytes)]),
  );
  const description = query<{ imports: { contract?: string }[] }>(contract, { kind: "describe" });
  for (const imported of description.imports) {
    if (!imported.contract) continue;
    const identity = /^([^:]+):([^/]+)\/([^@]+)(?:@[^@]+)?$/.exec(imported.contract);
    if (!identity) continue;
    const [, namespace, name, interfaceName] = identity;
    const declaration = `interfaces/${namespace}-${name}-${interfaceName}.d.ts`;
    const contents = generatedFiles.get(declaration);
    if (!contents) continue;
    // The pinned generator can collapse two imported versions onto one file. Never alias
    // that file to the other identity: that would silently change its binary signature.
    const declared = /@module Interface ([^\s*]+)/.exec(contents)?.[1];
    if (declared !== imported.contract)
      throw new Error(
        `Ambiguous generated WIT interface: ${imported.contract} resolves to ${declared}`,
      );
    const closure = declarationClosure(declaration, generatedFiles);
    const fingerprint = createHash("sha256").update(imported.contract);
    for (const [name, contents] of [...closure].sort(([a], [b]) => a.localeCompare(b)))
      fingerprint.update(JSON.stringify([name, contents]));
    const digest = fingerprint.digest("hex");
    imports.set(imported.contract, digest);
    const shared = path.join(root, ".tembra", "imports", digest);
    for (const [name, contents] of closure) files.set(path.join(shared, name), contents);
    const module = "./" + declaration.replace(/\.d\.ts$/, ".js");
    const ambient = path.join(shared, "index.d.ts");
    files.set(ambient, canonicalModule(imported.contract, module, contents));
    const reference = "./" + path.relative(path.dirname(alias), ambient).split(path.sep).join("/");
    references.push(`/// <reference path=${JSON.stringify(reference)} />`);
  }
  files.set(alias, [...references, `export * from ${JSON.stringify(target)};`, ""].join("\n"));
  return { files, sha256, contract, imports };
}

/** Keep the imported interface's complete official type graph, excluding unrelated world exports. */
function declarationClosure(
  entry: string,
  generated: ReadonlyMap<string, string>,
): Map<string, string> {
  const files = new Map<string, string>();
  function include(file: string): void {
    if (files.has(file)) return;
    const contents = generated.get(file);
    if (contents === undefined) throw new Error(`Missing generated WIT declaration: ${file}`);
    files.set(file, contents);
    const source = ts.createSourceFile(file, contents, ts.ScriptTarget.ESNext, true);
    function visit(node: ts.Node): void {
      const specifier =
        ts.isImportTypeNode(node) && ts.isLiteralTypeNode(node.argument)
          ? node.argument.literal
          : ts.isImportDeclaration(node) || ts.isExportDeclaration(node)
            ? node.moduleSpecifier
            : undefined;
      if (specifier && ts.isStringLiteral(specifier) && specifier.text.startsWith("."))
        include(
          path.posix
            .normalize(path.posix.join(path.posix.dirname(file), specifier.text))
            .replace(/\.js$/, ".d.ts"),
        );
      ts.forEachChild(node, visit);
    }
    visit(source);
  }
  include(entry);
  return files;
}

/** Alias official declarations without copying their signatures or defining another WIT schema. */
function canonicalModule(identity: string, module: string, contents: string): string {
  const source = ts.createSourceFile(module, contents, ts.ScriptTarget.ESNext, true);
  const types = new Map<string, readonly ts.TypeParameterDeclaration[]>();
  const values = new Set<string>();
  for (const statement of source.statements) {
    if (
      !ts.canHaveModifiers(statement) ||
      !ts.getModifiers(statement)?.some((modifier) => modifier.kind === ts.SyntaxKind.ExportKeyword)
    )
      continue;
    if (ts.isInterfaceDeclaration(statement) || ts.isTypeAliasDeclaration(statement))
      types.set(statement.name.text, statement.typeParameters ?? []);
    else if (ts.isFunctionDeclaration(statement) && statement.name) values.add(statement.name.text);
    else if (
      (ts.isClassDeclaration(statement) || ts.isEnumDeclaration(statement)) &&
      statement.name
    ) {
      types.set(
        statement.name.text,
        ts.isClassDeclaration(statement) ? (statement.typeParameters ?? []) : [],
      );
      values.add(statement.name.text);
    } else throw new Error(`Unsupported generated WIT declaration in ${identity}`);
  }
  const from = `import(${JSON.stringify(module)})`;
  return [
    `declare module ${JSON.stringify(identity)} {`,
    ...[...types].map(([name, parameters]) => {
      const declaration = parameters.length
        ? `<${parameters.map((parameter) => parameter.getText(source)).join(", ")}>`
        : "";
      const arguments_ = parameters.length
        ? `<${parameters.map((parameter) => parameter.name.text).join(", ")}>`
        : "";
      return `  export type ${name}${declaration} = ${from}.${name}${arguments_};`;
    }),
    ...[...values].map((name) => `  export const ${name}: typeof ${from}.${name};`),
    "}",
  ].join("\n");
}
