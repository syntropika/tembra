import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { embeddedAssets } from "./embedded-assets.js";

let materialized: string | undefined;

/** Resolve bundled compiler/SDK resources without consulting the caller's project installation. */
export function runtimeAssets(): {
  sdk: string;
  browser: string;
  compiler: string;
  types?: string;
  directory?: string;
} {
  if (embeddedAssets === undefined) {
    return {
      sdk: fileURLToPath(import.meta.resolve("@tembra/sdk")),
      browser: fileURLToPath(import.meta.resolve("@tembra/sdk/browser")),
      compiler: fileURLToPath(import.meta.resolve("typescript")),
    };
  }
  if (materialized === undefined) {
    const directory = mkdtempSync(path.join(tmpdir(), "tembra-assets-"));
    try {
      for (const [relative, content] of Object.entries(embeddedAssets)) {
        const destination = path.join(directory, relative);
        mkdirSync(path.dirname(destination), { recursive: true });
        writeFileSync(destination, content);
      }
      materialized = directory;
      process.once("exit", () => {
        rmSync(directory, { recursive: true, force: true });
      });
    } catch (error) {
      rmSync(directory, { recursive: true, force: true });
      throw error;
    }
  }
  return {
    sdk: path.join(materialized, "node_modules/@tembra/sdk/dist/src/index.js"),
    browser: path.join(materialized, "node_modules/@tembra/sdk/dist/src/browser/index.js"),
    compiler: path.join(materialized, "node_modules/typescript/lib/typescript.js"),
    types: path.join(materialized, "node_modules/@types"),
    directory: materialized,
  };
}
