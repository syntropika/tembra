import { createHash } from "node:crypto";
import { builtinModules } from "node:module";
import { mkdir, readFile, rename, writeFile } from "node:fs/promises";
import fs from "node:fs";
import path from "node:path";
import resolve from "enhanced-resolve";
import ts from "typescript";

export const Platform = Object.freeze({
  Browser: "browser",
  Worker: "worker",
  Node: "node",
  Embedded: "embedded",
} as const);
export type Platform = (typeof Platform)[keyof typeof Platform];
export interface BuildTarget {
  entry: string;
  platform: Platform;
  minify?: boolean;
  typecheck?: boolean;
  nativeExternals?: readonly string[];
  define?: Readonly<Record<string, string>>;
}
export interface BuildConfig {
  root?: string;
  outDir?: string;
  builds: Readonly<Record<string, BuildTarget>>;
}
export interface Artifact {
  name: string;
  platform: Platform;
  path: string;
  sha256: string;
  bytes: number;
}

export function defineConfig<const C extends BuildConfig>(config: C): C {
  return config;
}

function external(specifier: string, target: BuildTarget): boolean {
  return (
    target.platform === Platform.Node &&
    (builtinModules.includes(specifier.replace(/^node:/, "")) ||
      (target.nativeExternals ?? []).some(
        (name) => specifier === name || specifier.startsWith(`${name}/`),
      ))
  );
}

function resolver(root: string, target: BuildTarget): Bun.BunPlugin {
  const web = target.platform === Platform.Browser || target.platform === Platform.Worker;
  const resolveModule = resolve.create.sync({
    fileSystem: fs,
    conditionNames: [
      ...(web
        ? ["browser"]
        : target.platform === Platform.Node
          ? ["node", "node-addons"]
          : ["tembra-embedded"]),
      "import",
      "default",
    ],
    extensions: [".ts", ".tsx", ".mts", ".js", ".jsx", ".mjs", ".cjs", ".json"],
    extensionAlias: { ".js": [".ts", ".tsx", ".js"], ".mjs": [".mts", ".mjs"] },
    mainFields: web ? ["browser", "module", "main"] : ["module", "main"],
    aliasFields: web ? ["browser"] : [],
  });
  return {
    name: "tembra-platform",
    setup(builder) {
      builder.onResolve({ filter: /^tembra:entry$/ }, () => ({
        path: "entry",
        namespace: "tembra-facet",
      }));
      builder.onLoad({ filter: /.*/, namespace: "tembra-facet" }, () => ({
        contents: `import facet from ${JSON.stringify(path.resolve(root, target.entry))}; globalThis.__tembraFacet = facet;`,
        loader: "js",
      }));
      builder.onResolve({ filter: /.*/ }, (request) => {
        if (external(request.path, target)) return { path: request.path, external: true };
        if (
          request.path.startsWith("node:") ||
          request.path.startsWith("bun:") ||
          builtinModules.includes(request.path)
        )
          throw new Error(`${request.importer}: ${request.path} requires its host runtime`);
        if (request.kind === "entry-point-build") return;
        const resolved = resolveModule(
          path.dirname(request.importer || path.join(root, "entry.js")),
          request.path,
        );
        if (!resolved)
          throw new Error(`${request.importer}: no compatible implementation for ${request.path}`);
        if (resolved.endsWith(".node"))
          throw new Error("Declare native Node packages as nativeExternals in a node build");
        return { path: resolved };
      });
      builder.onLoad({ filter: /\.[cm]?[jt]sx?$/ }, async (request) => {
        const source = await readFile(request.path, "utf8");
        const parsed = ts.createSourceFile(request.path, source, ts.ScriptTarget.Latest, true);
        function check(node: ts.Node): void {
          if (
            ts.isCallExpression(node) &&
            (node.expression.kind === ts.SyntaxKind.ImportKeyword ||
              (ts.isIdentifier(node.expression) && node.expression.text === "require"))
          ) {
            const argument = node.arguments[0];
            if (!argument || !ts.isStringLiteralLike(argument))
              throw new Error(
                `${request.path}: dynamic module names must be resolved before publishing a facet`,
              );
          }
          ts.forEachChild(node, check);
        }
        check(parsed);
        return undefined;
      });
    },
  };
}

function typecheck(entry: string, root: string): void {
  const configFile = ts.findConfigFile(root, ts.sys.fileExists);
  const loaded = configFile ? ts.readConfigFile(configFile, ts.sys.readFile) : { config: {} };
  if (loaded.error)
    throw new Error(ts.flattenDiagnosticMessageText(loaded.error.messageText, "\n"));
  const parsed = ts.parseJsonConfigFileContent(loaded.config, ts.sys, root);
  const program = ts.createProgram([entry], {
    ...parsed.options,
    target: ts.ScriptTarget.ES2023,
    module: ts.ModuleKind.ESNext,
    moduleResolution: ts.ModuleResolutionKind.Bundler,
    allowImportingTsExtensions: true,
    noEmit: true,
    strict: true,
    skipLibCheck: true,
  });
  const diagnostics = ts.getPreEmitDiagnostics(program);
  if (diagnostics.length)
    throw new Error(
      ts.formatDiagnosticsWithColorAndContext(diagnostics, {
        getCurrentDirectory: () => root,
        getCanonicalFileName: (name) => name,
        getNewLine: () => "\n",
      }),
    );
}

/** Resolves packages at build time and emits exactly one content-addressed ESM file per target. */
export async function build(config: BuildConfig): Promise<readonly Artifact[]> {
  const root = path.resolve(config.root ?? process.cwd());
  const outDir = path.resolve(root, config.outDir ?? "dist/tembra");
  const artifacts: Artifact[] = [];
  for (const [name, target] of Object.entries(config.builds)) {
    if (!/^[\w-]+$/.test(name) || !Object.values(Platform).includes(target.platform))
      throw new Error("Invalid build name or platform");
    const entry = path.resolve(root, target.entry);
    if (target.typecheck !== false) typecheck(entry, root);
    const result = await Bun.build({
      entrypoints: [target.platform === Platform.Embedded ? "tembra:entry" : entry],
      root,
      format: target.platform === Platform.Embedded ? "iife" : "esm",
      target: target.platform === Platform.Node ? "node" : "browser",
      splitting: false,
      minify: target.minify ?? true,
      ...(target.define ? { define: { ...target.define } } : {}),
      plugins: [resolver(root, target)],
    });
    if (!result.success) throw new AggregateError(result.logs, `Build ${name} failed`);
    if (result.outputs.length !== 1 || result.outputs[0]?.kind !== "entry-point")
      throw new Error(
        `Build ${name} requires external assets; publish them explicitly instead of silently expanding the facet bundle`,
      );
    const output = result.outputs[0]!;
    const source = await output.text();
    const parsed = ts.createSourceFile(
      "bundle.js",
      source,
      ts.ScriptTarget.Latest,
      true,
      ts.ScriptKind.JS,
    );
    const hasDefault = parsed.statements.some(
      (statement) =>
        ts.isExportAssignment(statement) ||
        (ts.isExportDeclaration(statement) &&
          statement.exportClause &&
          ts.isNamedExports(statement.exportClause) &&
          statement.exportClause.elements.some((element) => element.name.text === "default")),
    );
    if (target.platform !== Platform.Embedded && !hasDefault)
      throw new Error(`Build ${name} must export a default facet`);
    function verify(node: ts.Node): void {
      let specifier: ts.Expression | undefined;
      if (ts.isImportDeclaration(node) || ts.isExportDeclaration(node))
        specifier = node.moduleSpecifier;
      if (ts.isCallExpression(node) && node.expression.kind === ts.SyntaxKind.ImportKeyword)
        specifier = node.arguments[0];
      if (specifier && (!ts.isStringLiteralLike(specifier) || !external(specifier.text, target)))
        throw new Error(`Build ${name} contains an unresolved import`);
      ts.forEachChild(node, verify);
    }
    verify(parsed);
    const bytes = Buffer.from(source);
    const hash = createHash("sha256").update(bytes).digest("hex");
    const directory = path.join(outDir, name);
    const destination = path.join(directory, `facet.${hash}.js`);
    await mkdir(directory, { recursive: true });
    const temporary = `${destination}.${process.pid}.tmp`;
    await writeFile(temporary, bytes);
    await rename(temporary, destination);
    artifacts.push({
      name,
      platform: target.platform,
      path: destination,
      sha256: hash,
      bytes: bytes.length,
    });
  }
  return artifacts;
}
