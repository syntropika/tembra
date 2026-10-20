import { createHash } from "node:crypto";

import type { EsmArtifactV1 } from "@tembra/sdk";
import { init, parse } from "es-module-lexer/js";
import ts from "typescript";

import { ToolingError } from "./error.js";
import type { Platform } from "./types.js";

const loaderLimit = 1_048_576;
const nodeGlobals = new Set([
  "process",
  "Buffer",
  "require",
  "module",
  "exports",
  "__dirname",
  "__filename",
  "global",
  "setImmediate",
  "clearImmediate",
  "Deno",
  "Bun",
]);
const webGlobals = new Set([
  "window",
  "document",
  "navigator",
  "location",
  "self",
  "fetch",
  "Request",
  "Response",
  "Headers",
  "URL",
  "URLSearchParams",
  "WebSocket",
  "EventSource",
  "Worker",
  "SharedWorker",
  "localStorage",
  "sessionStorage",
  "indexedDB",
  "caches",
  "crypto",
  "TextEncoder",
  "TextDecoder",
  "AbortController",
  "AbortSignal",
  "setTimeout",
  "clearTimeout",
  "setInterval",
  "clearInterval",
  "queueMicrotask",
  "requestAnimationFrame",
  "cancelAnimationFrame",
  "console",
  "Blob",
  "File",
  "FormData",
  "ReadableStream",
  "WritableStream",
  "TransformStream",
  "performance",
  "atob",
  "btoa",
]);

function propertyName(node: ts.Identifier): boolean {
  const { parent } = node;
  return (
    (ts.isPropertyAccessExpression(parent) && parent.name === node) ||
    ((ts.isPropertyAssignment(parent) || ts.isMethodDeclaration(parent)) && parent.name === node) ||
    (ts.isBindingElement(parent) && parent.propertyName === node)
  );
}

/**
 * Refuse known unprovided platform globals before loading a bundle. Server selects embedded V8,
 * so Node and browser-only globals are unavailable; browser targets still refuse Node globals.
 * A direct typeof check of an absent identifier is permitted because it needs no platform API.
 * This is conservative static inspection, not evaluation or a JavaScript security sandbox.
 * A refusal throws ToolingError artifact/platform_global, including target when supplied.
 */
export function inspectPlatform(source: string, platform: Platform, target?: string): void {
  const filename = "/tembra-artifact.js";
  const options: ts.CompilerOptions = {
    allowJs: true,
    noLib: true,
    noResolve: true,
    target: ts.ScriptTarget.ES2022,
  };
  const host = ts.createCompilerHost(options);
  const file = ts.createSourceFile(
    filename,
    source,
    ts.ScriptTarget.ES2022,
    true,
    ts.ScriptKind.JS,
  );
  host.getSourceFile = (name) => (name === filename ? file : undefined);
  host.fileExists = (name) => name === filename;
  host.readFile = (name) => (name === filename ? source : undefined);
  const checker = ts.createProgram([filename], options, host).getTypeChecker();
  const forbidden = (name: string): boolean =>
    nodeGlobals.has(name) || (platform === "server" && webGlobals.has(name));
  const isUnbound = (node: ts.Identifier): boolean => {
    const symbol = checker.getSymbolAtLocation(node);
    return (
      symbol === undefined ||
      symbol.declarations?.some((declaration) => declaration.getSourceFile() === file) !== true
    );
  };
  const refuse = (name: string, node: ts.Node): never => {
    const location = file.getLineAndCharacterOfPosition(node.getStart(file));
    throw new ToolingError(
      "artifact",
      "platform_global",
      `${platform} executor does not provide ${name} (${location.line + 1}:${location.character + 1})`,
      target,
    );
  };
  const inspectProperty = (node: ts.Node): void => {
    if (!ts.isPropertyAccessExpression(node) && !ts.isElementAccessExpression(node)) {
      return;
    }
    const root = node.expression;
    if (
      !ts.isIdentifier(root) ||
      !["globalThis", "window", "self", "global"].includes(root.text) ||
      !isUnbound(root)
    ) {
      return;
    }
    let name: string | undefined;
    if (ts.isPropertyAccessExpression(node)) {
      name = node.name.text;
    } else if (ts.isStringLiteralLike(node.argumentExpression)) {
      name = node.argumentExpression.text;
    }
    if (name !== undefined && forbidden(name)) {
      refuse(name, node);
    }
  };
  const inspectBinding = (node: ts.Node): void => {
    if (
      !ts.isVariableDeclaration(node) ||
      !ts.isObjectBindingPattern(node.name) ||
      node.initializer === undefined
    ) {
      return;
    }
    const root = node.initializer;
    if (!ts.isIdentifier(root) || root.text !== "globalThis" || !isUnbound(root)) {
      return;
    }
    for (const element of node.name.elements) {
      const key = element.propertyName ?? element.name;
      if ((ts.isIdentifier(key) || ts.isStringLiteralLike(key)) && forbidden(key.text)) {
        refuse(key.text, key);
      }
    }
  };
  const visit = (node: ts.Node): void => {
    if (
      ts.isIdentifier(node) &&
      forbidden(node.text) &&
      isUnbound(node) &&
      !propertyName(node) &&
      !ts.isTypeOfExpression(node.parent)
    ) {
      refuse(node.text, node);
    }
    inspectProperty(node);
    inspectBinding(node);
    ts.forEachChild(node, visit);
  };
  visit(file);
}

/**
 * Validates the closed module profile and derives its loader envelope from final UTF-8 bytes.
 * Refusal performs no file writes or executor evaluation. Static platform checks are conservative;
 * successful construction does not guarantee evaluation or facet compatibility in an executor.
 * Identity must be nonempty scalar text without control characters. selectedExport must exist;
 * imports, import.meta and an exported then are refused. maxBytes is a positive UTF-8 byte limit,
 * defaults to 1,048,576 and cannot raise that loader maximum. The returned envelope and exports list
 * are frozen snapshots; source itself remains unchanged. Optional target labels diagnostic errors.
 */
export async function createArtifact(
  source: string,
  identity: string,
  selectedExport: string,
  platform: Platform,
  maxBytes = loaderLimit,
  target?: string,
): Promise<EsmArtifactV1> {
  if (!Number.isSafeInteger(maxBytes) || maxBytes <= 0) {
    throw new ToolingError(
      "artifact",
      "source_limit",
      "source limit must be a positive safe integer",
      target,
    );
  }
  if (
    !identity ||
    /\p{Cc}/u.test(identity) ||
    Buffer.from(identity).toString("utf-8") !== identity ||
    Buffer.from(source).toString("utf-8") !== source
  ) {
    throw new ToolingError(
      "artifact",
      "metadata",
      "artifact identity/source must contain valid scalar text",
      target,
    );
  }
  if (Buffer.byteLength(source, "utf-8") > Math.min(maxBytes, loaderLimit)) {
    throw new ToolingError(
      "artifact",
      "source_size",
      "bundled source exceeds the configured or loader source limit",
      target,
    );
  }
  await init();
  let result: ReturnType<typeof parse>;
  try {
    result = parse(source);
  } catch (error) {
    throw new ToolingError("artifact", "syntax", "bundled source could not be analyzed", target, {
      cause: error,
    });
  }
  if (result[0].length !== 0) {
    throw new ToolingError(
      "artifact",
      "imports",
      "imports and import.meta are outside the closed module profile",
      target,
    );
  }
  const names = result[1]
    .map((entry) => {
      if (!("name" in entry)) {
        throw new ToolingError(
          "artifact",
          "exports",
          "re-exports are outside the closed module profile",
          target,
        );
      }
      return entry.name;
    })
    .toSorted();
  if (
    names.includes("then") ||
    new Set(names).size !== names.length ||
    !names.includes(selectedExport)
  ) {
    throw new ToolingError(
      "artifact",
      "exports",
      `unsupported exports or missing selected export ${selectedExport}`,
      target,
    );
  }
  inspectPlatform(source, platform, target);
  return Object.freeze({
    format: "tembra-esm-v1",
    identity,
    source,
    sha256: createHash("sha256").update(source, "utf-8").digest("hex"),
    exports: Object.freeze(names),
  });
}
