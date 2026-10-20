import { Buffer } from "node:buffer";
import { readFileSync } from "node:fs";
import { readFile, realpath } from "node:fs/promises";
import { createRequire } from "node:module";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { Script, createContext } from "node:vm";

import { sha256 } from "@noble/hashes/sha2.js";
import { createJiti } from "jiti";
import type { TransformOptions, TransformResult } from "jiti";

import { executorFailure } from "../host/error.js";
import type { FacetModuleLoader } from "../host/loader.js";
import * as sdk from "../index.js";
import type { Preparation } from "./loader.js";

type Transform = (options: TransformOptions) => TransformResult;
let compiler: Transform | undefined;

/** Babel's bootstrap runs in a compiler-only realm. Facets always execute in the Node realm. */
function sourceCompiler(): Transform {
  if (compiler) {
    return compiler;
  }
  const require = createRequire(import.meta.url);
  const manifest = require.resolve("jiti/package.json");
  const metadata: unknown = JSON.parse(readFileSync(manifest, "utf-8"));
  if (
    metadata === null ||
    typeof metadata !== "object" ||
    !("version" in metadata) ||
    metadata.version !== "2.7.0"
  ) {
    throw executorFailure(
      "source_compiler",
      "the qualified Jiti 2.7.0 compiler is required",
      "unavailable",
    );
  }
  // Pinned package asset: fail explicitly if a future package changes this compiler distribution.
  const filename = path.join(path.dirname(manifest), "dist/babel.cjs");
  const context = createContext({ process, Buffer, console, setTimeout, clearTimeout });
  const module = { exports: {} as unknown };
  const bootstrap: unknown = new Script(
    `(function(exports,require,module,__filename,__dirname){${readFileSync(filename, "utf-8")}\n})`,
    { filename },
  ).runInContext(context);
  if (typeof bootstrap !== "function") {
    throw executorFailure("source_compiler", "compiler bootstrap is invalid");
  }
  Reflect.apply(bootstrap, undefined, [
    module.exports,
    createRequire(filename),
    module,
    filename,
    path.dirname(filename),
  ]);
  if (typeof module.exports !== "function") {
    throw executorFailure("source_compiler", "compiler export is invalid");
  }
  // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- The pinned compiler function is checked above and implements Jiti's public transform hook.
  compiler = module.exports as Transform;
  return compiler;
}

export type SourceModule = {
  readonly filename: string;
  readonly source: string;
  readonly bytes: number;
  readonly hash: string;
};
export async function captureSource(specifier: string, base: URL): Promise<SourceModule> {
  let url: URL;
  try {
    url =
      /^[a-z][a-z0-9+.-]*:/iu.test(specifier) && !path.isAbsolute(specifier)
        ? new URL(specifier)
        : pathToFileURL(path.resolve(fileURLToPath(base), specifier));
  } catch {
    throw executorFailure("module_url", "invalid Node module path or file URL");
  }
  if (url.protocol !== "file:" || url.search || url.hash) {
    throw executorFailure(
      "module_url",
      "Node source loading accepts local file paths and file URLs without queries or fragments",
    );
  }
  let filename: string;
  let source: string;
  try {
    filename = await realpath(fileURLToPath(url));
    source = await readFile(filename, "utf-8");
  } catch (error) {
    throw executorFailure(
      "module_read",
      error instanceof Error ? error.message : "module could not be read",
      "unavailable",
    );
  }
  if (!/\.(?:[cm]?ts|m?js)$/u.test(filename)) {
    throw executorFailure("module_format", "source entry must be .ts, .mts, .cts, .js or .mjs");
  }
  const bytes = Buffer.byteLength(source);
  if (bytes > 2_097_152) {
    throw executorFailure("source_size", "source entry exceeds 2 MiB");
  }
  const hash = Array.from(sha256(Buffer.from(source)), (byte) =>
    byte.toString(16).padStart(2, "0"),
  ).join("");
  return { filename, source, bytes, hash };
}

export function nodeModuleBase(input?: string | URL): URL {
  const cwd = pathToFileURL(path.resolve(".") + path.sep);
  let result = cwd;
  if (input !== undefined) {
    const text = String(input);
    try {
      result =
        /^[a-z][a-z0-9+.-]*:/iu.test(text) && !path.isAbsolute(text)
          ? new URL(text)
          : pathToFileURL(path.resolve(fileURLToPath(cwd), text) + path.sep);
    } catch {
      throw executorFailure("module_url", "invalid Node moduleBase path or file URL");
    }
  }
  if (result.protocol !== "file:" || result.search || result.hash) {
    throw executorFailure("module_url", "moduleBase must be a local directory URL");
  }
  if (!result.pathname.endsWith("/")) {
    result.pathname += "/";
  }
  return result;
}

/** Fresh per-preparation module graph; native packages/builtins retain normal Node cache semantics. */
export function sourceEvaluator(
  preparation: Preparation,
): FacetModuleLoader<SourceModule> & { readonly close: () => void } {
  const transform = sourceCompiler();
  const cache = {};
  let active = true;
  let entry: string | undefined;
  const jiti = createJiti(import.meta.url, {
    moduleCache: false,
    fsCache: false,
    interopDefault: false,
    tryNative: false,
    tsconfigPaths: false,
    alias: {},
    sourceMaps: false,
    jsx: false,
    nativeModules: ["@tembra/sdk"],
    virtualModules: { "@tembra/sdk": sdk },
    transformModules: [],
    transform(options) {
      if (!active) {
        throw executorFailure(
          "source_preparation_ended",
          "new source transforms require host.load or host.reload",
        );
      }
      if (options.filename === entry) {
        entry = undefined;
      } else {
        preparation.chargeSourceModule(Buffer.byteLength(options.source));
      }
      const result = transform(options);
      if (result.error !== undefined) {
        throw executorFailure(
          "module_transform",
          `source module cannot be transformed: ${String(result.error)}`,
        );
      }
      return result;
    },
  });
  return {
    evaluate(module: SourceModule): { readonly namespace: unknown } {
      entry = module.filename;
      // Synchronous Jiti transformation keeps local .mjs and TypeScript dependencies in this graph.
      // This explicit source profile does not support top-level await or native ESM equivalence.
      const namespace: unknown = jiti.evalModule(module.source, {
        filename: module.filename,
        async: false,
        forceTranspile: true,
        cache,
      });
      return { namespace };
    },
    close() {
      active = false;
    },
  };
}
