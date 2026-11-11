import resolve from "enhanced-resolve";
import fs from "node:fs";
import { readFile } from "node:fs/promises";
import { builtinModules } from "node:module";
import path from "node:path";
import ts from "typescript";
import type { WitImports } from "./types.ts";

/** A WIT interface specifier, `namespace:package/interface[@version]`. */
export const WIT_SPECIFIER = /^[a-z][a-z0-9-]*:[a-z][a-z0-9-]*\/[a-z][a-z0-9-]*(@[\w.+-]+)?$/;

const ENTRY_NAMESPACE = "tembra-orb";
/** The generated entry module path; author modules always have absolute paths. */
const ENTRY_PATH = "tembra-generated-entry";

function runtimeModule(specifier: string): boolean {
  return (
    specifier.startsWith("node:") ||
    specifier.startsWith("bun:") ||
    builtinModules.includes(specifier)
  );
}

/** Resolve and bundle dependencies under the component runtime's rules. */
export function resolver(root: string, entry: string, imports: WitImports): Bun.BunPlugin {
  const resolveModule = resolve.create.sync({
    fileSystem: fs,
    conditionNames: ["tembra-wasm", "source", "import", "default"],
    extensions: [".ts", ".tsx", ".mts", ".js", ".jsx", ".mjs", ".cjs", ".json"],
    extensionAlias: { ".js": [".ts", ".tsx", ".js"], ".mjs": [".mts", ".mjs"] },
    mainFields: ["module", "main"],
    aliasFields: [],
  });
  return {
    name: "tembra-platform",
    /** Register the generated entrypoint, external WIT imports and package resolution. */
    setup(builder) {
      builder.onResolve({ filter: /^tembra:entry$/ }, () => ({
        path: ENTRY_PATH,
        namespace: ENTRY_NAMESPACE,
      }));
      builder.onLoad({ filter: /.*/, namespace: ENTRY_NAMESPACE }, () => ({
        contents: entry,
        loader: "js",
        resolveDir: root,
      }));
      builder.onResolve({ filter: /.*/ }, (request) => {
        if (runtimeModule(request.path))
          throw new Error(`${request.importer}: ${request.path} requires its host runtime`);
        const generated = request.namespace === ENTRY_NAMESPACE || request.importer === ENTRY_PATH;
        if (WIT_SPECIFIER.test(request.path)) {
          // The component runtime resolves WIT imports natively; they stay ES imports.
          if (generated && imports.entry.has(request.path))
            return { path: request.path, external: true };
          if (imports.direct.has(request.path)) return { path: request.path, external: true };
          if (request.path.startsWith("tembra:orb/"))
            throw new Error(
              `${request.importer}: ${request.path} is private to the Tembra runtime`,
            );
          if (imports.entry.has(request.path))
            throw new Error(
              `${request.importer}: ${request.path} is an Orb dependency; select it with Declaration.get or Context.get`,
            );
          throw new Error(`${request.importer}: the Orb contract does not import ${request.path}`);
        }
        if (request.kind === "entry-point-build") return;
        const resolved = resolveModule(
          path.dirname(
            generated || !request.importer ? path.join(root, "entry.js") : request.importer,
          ),
          request.path,
        );
        if (!resolved)
          throw new Error(`${request.importer}: no compatible implementation for ${request.path}`);
        if (resolved.endsWith(".node"))
          throw new Error("Native Node packages cannot run inside an Orb");
        return { path: resolved };
      });
      builder.onLoad({ filter: /\.[cm]?[jt]sx?$/ }, async (request) => {
        const source = await readFile(request.path, "utf8");
        const parsed = ts.createSourceFile(request.path, source, ts.ScriptTarget.Latest, true);
        /** Module names must be static so every import is resolved before publishing. */
        function check(node: ts.Node): void {
          if (
            ts.isCallExpression(node) &&
            (node.expression.kind === ts.SyntaxKind.ImportKeyword ||
              (ts.isIdentifier(node.expression) && node.expression.text === "require"))
          ) {
            const argument = node.arguments[0];
            if (!argument || !ts.isStringLiteralLike(argument))
              throw new Error(
                `${request.path}: dynamic module names must be resolved before publishing an orb`,
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
