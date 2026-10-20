import * as fs from "node:fs";
import path from "node:path";

import resolve from "enhanced-resolve";
import ts from "typescript";

import { runtimeAssets } from "./runtime-assets.js";

/** Capture every physical bundled input without changing Bun's loader behavior. */
export function collectBundleInputs(result: Bun.BuildOutput, inputs: Set<string>): void {
  if (result.metafile === undefined) {
    throw new Error("Bun did not return requested bundle input metadata");
  }
  for (const file of Object.keys(result.metafile.inputs)) {
    // Namespaced empty modules have source-map entries but no filesystem ownership.
    if (/^[A-Za-z][A-Za-z0-9+.-]*:/u.test(file) && !path.isAbsolute(file)) {
      continue;
    }
    inputs.add(path.resolve(process.cwd(), file));
  }
}

/** Force root and browser entrypoints through one canonical SDK installation. */
export function canonicalSdk(external = false): Bun.BunPlugin {
  const assets = runtimeAssets();
  return {
    name: "tembra-single-sdk",
    setup(builder) {
      builder.onResolve({ filter: /^@tembra\/sdk(?:\/browser)?$/u }, (request) => {
        if (external) {
          return { path: request.path, external: true };
        }
        return {
          path: request.path.endsWith("/browser") ? assets.browser : assets.sdk,
        };
      });
    },
  };
}

/** A failed native build retains the complete diagnostics as its original cause. */
export function checkedBundle(result: Bun.BuildOutput, root = process.cwd()): Bun.BuildOutput {
  if (!result.success) {
    const messages = result.logs.map((message) => {
      const { position } = message;
      if (position === null) {
        return message.message;
      }
      const file = path.relative(root, position.file);
      if (position.line === 0 && message.message.startsWith(`${position.file}:`)) {
        // JS resolver hooks receive no source position; our refusal already carries the import site.
        return `${file}:${message.message.slice(position.file.length + 1)}`;
      }
      const column = position.line > 0 ? position.column : position.column + 1;
      return `${file}:${Math.max(1, position.line)}:${column}: ${message.message}`;
    });
    throw new AggregateError(result.logs, messages.join("\n"));
  }
  return result;
}

function importFailure(request: Bun.OnResolveArgs, error: unknown): Error {
  let line = 1;
  let column = 1;
  try {
    const text = fs.readFileSync(request.importer, "utf-8");
    const file = ts.createSourceFile(request.importer, text, ts.ScriptTarget.Latest, true);
    const visit = (node: ts.Node): number | undefined => {
      if (ts.isStringLiteralLike(node) && node.text === request.path) {
        const { parent } = node;
        const imported =
          ((ts.isImportDeclaration(parent) || ts.isExportDeclaration(parent)) &&
            parent.moduleSpecifier === node) ||
          (ts.isExternalModuleReference(parent) && parent.expression === node) ||
          (ts.isCallExpression(parent) &&
            parent.arguments[0] === node &&
            (parent.expression.kind === ts.SyntaxKind.ImportKeyword ||
              (ts.isIdentifier(parent.expression) && parent.expression.text === "require") ||
              (ts.isPropertyAccessExpression(parent.expression) &&
                ts.isIdentifier(parent.expression.expression) &&
                parent.expression.expression.text === "require" &&
                parent.expression.name.text === "resolve")));
        if (imported) {
          return node.getStart(file) + 1;
        }
      }
      return ts.forEachChild(node, visit);
    };
    const offset = visit(file);
    if (offset !== undefined) {
      const position = file.getLineAndCharacterOfPosition(offset);
      line = position.line + 1;
      column = position.character + 1;
    }
  } catch {
    // A missing importer still has a useful file-level location; retain the original failure below.
  }
  return new Error(
    `${request.importer || request.path}:${line}:${column}: ${error instanceof Error ? error.message : String(error)}`,
    { cause: error },
  );
}

/** Facets execute in the bounded V8/browser profile and cannot import host runtime builtins. */
export function rejectHostBuiltins(): Bun.BunPlugin {
  return {
    name: "tembra-no-host-builtins",
    setup(builder) {
      builder.onResolve({ filter: /.*/u }, (request) => {
        if (request.path.startsWith("node:") || request.path.startsWith("bun:")) {
          throw importFailure(
            request,
            new Error(`Facet code cannot import host runtime module ${request.path}`),
          );
        }
        return undefined;
      });
    },
  };
}

/** Resolve every facet dependency before Bun can supply implicit host modules or polyfills. */
export function facetResolution(
  tsconfig: string,
  conditions: readonly string[],
  inputs: Set<string>,
  platform: "server" | "browser",
): Bun.BunPlugin {
  const options: resolve.ResolveOptionsOptionalFS = {
    fileSystem: fs,
    // Each build owns uncached filesystem observations; watch rebuilds must see changed manifests.
    mainFields: platform === "browser" ? ["browser", "module", "main"] : ["module", "main"],
    aliasFields: platform === "browser" ? ["browser"] : [],
    extensions: [".tsx", ".ts", ".jsx", ".js", ".mjs", ".mts", ".cts", ".cjs", ".json"],
    extensionAlias: {
      ".js": [".ts", ".tsx", ".js", ".jsx"],
      ".jsx": [".tsx", ".jsx"],
      ".mjs": [".mts", ".mjs"],
      ".cjs": [".cts", ".cjs"],
    },
    tsconfig: fs.existsSync(tsconfig) ? tsconfig : false,
  };
  const imported = resolve.create.sync({ ...options, conditionNames: [...conditions, "import"] });
  const required = resolve.create.sync({ ...options, conditionNames: [...conditions, "require"] });
  return {
    name: "tembra-facet-resolution",
    setup(builder) {
      builder.onResolve({ filter: /.*/u }, (request) => {
        // Enhanced-resolve parses # and ? inside request strings as URL fragments/queries.
        // Bun already has the actual absolute entry filename and handles those characters literally.
        if (request.kind === "entry-point-build" && path.isAbsolute(request.path)) {
          return undefined;
        }
        const resolver =
          request.kind === "require-call" || request.kind === "require-resolve"
            ? required
            : imported;
        const observed = new Set<string>();
        let resolved: string | false;
        try {
          resolved = resolver(path.dirname(request.importer || tsconfig), request.path, {
            fileDependencies: observed,
          });
        } catch (error) {
          throw importFailure(request, error);
        }
        // The resolver also records traversed directories in fileDependencies; watchers hash files.
        for (const file of observed) {
          if (fs.statSync(file, { throwIfNoEntry: false })?.isFile() === true) {
            inputs.add(file);
          }
        }
        // Enhanced-resolve escapes literal # in rendered paths; Bun needs the physical filename.
        return resolved === false
          ? {
              path: JSON.stringify([
                bundleSourceName(request.importer, path.dirname(tsconfig)),
                request.path,
              ]),
              namespace: "tembra-empty",
            }
          : { path: resolved.replaceAll("\0#", "#") };
      });
      builder.onLoad({ filter: /.*/u, namespace: "tembra-empty" }, () => ({
        contents: "module.exports = {};",
        loader: "js",
      }));
    },
  };
}

/** Rebase Bun's cwd-relative map entries to the project root or a stable embedded SDK URL. */
export function bundleSourceName(source: string, root: string): string {
  if (/^[A-Za-z][A-Za-z0-9+.-]*:/u.test(source) && !path.isAbsolute(source)) {
    return source;
  }
  const absolute = path.resolve(process.cwd(), source);
  const { directory } = runtimeAssets();
  if (directory !== undefined) {
    const relative = path.relative(directory, absolute);
    if (relative !== ".." && !relative.startsWith(`..${path.sep}`) && !path.isAbsolute(relative)) {
      return `tembra:embedded/${relative.split(path.sep).join("/")}`;
    }
  }
  return path.relative(root, absolute).split(path.sep).join("/");
}

/** Normalize only parsed module-path comments; authored strings and executable code are retained. */
export function stableBundleSource(
  source: string,
  root: string,
  sourcemap: boolean,
  inputs: ReadonlySet<string>,
): string {
  const parsed = ts.createSourceFile(
    "facet.js",
    source,
    ts.ScriptTarget.Latest,
    false,
    ts.ScriptKind.JS,
  );
  const changes = new Map<number, { end: number; text: string }>();
  const visit = (node: ts.Node): void => {
    for (const comment of ts.getLeadingCommentRanges(source, node.getFullStart()) ?? []) {
      if (comment.kind !== ts.SyntaxKind.SingleLineCommentTrivia) {
        continue;
      }
      const text = source.slice(comment.pos, comment.end);
      if (!text.startsWith("// ")) {
        continue;
      }
      const filename = text.slice(3);
      // Bun emits its module headers relative to cwd, even with an explicit build root.
      // Match actual inputs so ordinary authored comments cannot become path-rewrite candidates.
      if (!inputs.has(path.resolve(process.cwd(), filename))) {
        continue;
      }
      const stable = bundleSourceName(filename, root);
      if (stable !== filename) {
        changes.set(comment.pos, { end: comment.end, text: `// ${stable}` });
      }
    }
    ts.forEachChild(node, visit);
  };
  visit(parsed);
  let normalized = source;
  for (const [start, change] of [...changes].toSorted(([left], [right]) => right - left)) {
    normalized = normalized.slice(0, start) + change.text + normalized.slice(change.end);
  }
  // Bun's terminal debugId depends on its temporary source paths. The matching map field is omitted.
  return sourcemap ? normalized.replace(/\n\/\/# debugId=[A-F0-9]+\n?$/u, "\n") : normalized;
}
