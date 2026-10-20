/**
 * Bun development tools for producing self-contained facets and delivering authorized updates.
 * Runtime facet code imports `@tembra/sdk`; browser development clients import
 * `@tembra/tooling/browser`. See the integrated tooling guide for the complete embedding example.
 *
 * @example
 * ```ts
 * import { build, loadConfig } from "@tembra/tooling";
 * const result = await build(await loadConfig("./tembra.config.ts"));
 * console.log(result.artifacts.map(output => output.artifactPath));
 * ```
 */
// oxlint-disable-next-line oxc/no-barrel-file -- This is the public Bun tooling entrypoint; runtime and browser entrypoints stay separate.
export type * from "./types.js";
export * from "./error.js";
export { defineConfig, loadConfig, resolveConfig } from "./config.js";
export * from "./codegen.js";
export * from "./build.js";
export * from "./dev.js";
export * from "./adapters.js";
