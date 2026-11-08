import resolve from "enhanced-resolve";
import fs from "node:fs";
import { readFile } from "node:fs/promises";
import { builtinModules } from "node:module";
import path from "node:path";
import ts from "typescript";

import type { BuildTarget } from "./types.ts";

/** Resolve and bundle dependencies under the selected runtime rules. */
export function resolver(root: string, target: BuildTarget): Bun.BunPlugin {
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
    /** Register target-specific resolution and embedded entrypoint rules. */
    setup(builder) {
      builder.onResolve({ filter: /^tembra:entry$/ }, () => ({
        path: "entry",
        namespace: "tembra-orb",
      }));
      builder.onLoad({ filter: /.*/, namespace: "tembra-orb" }, () => ({
        contents: `import orb from ${JSON.stringify(path.resolve(root, target.entry))}; globalThis.__tembraOrb = orb;`,
        loader: "js",
      }));
      builder.onResolve({ filter: /.*/ }, (request) => {
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
          throw new Error("Native Node packages cannot run inside an Orb");
        return { path: resolved };
      });
      builder.onLoad({ filter: /\.[cm]?[jt]sx?$/ }, async (request) => {
        const source = await readFile(request.path, "utf8");
        const parsed = ts.createSourceFile(request.path, source, ts.ScriptTarget.Latest, true);
        /** Reject browser and Node facilities absent from the embedded runtime. */
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
