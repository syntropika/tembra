import { defineConfig } from "oxfmt";

export default defineConfig({
  printWidth: 100,
  // Preserve the pinned WASI table and upstream guest helper templates in their source format.
  ignorePatterns: [
    "**/node_modules/**",
    "**/dist/**",
    "**/target/**",
    "**/*.lock",
    "**/sdk/src/interpreter/standard.ts",
    "**/node-binding/src/componentize/js/stream-from.js",
    "**/node-binding/src/componentize/js/future-from.js",
  ],
});
