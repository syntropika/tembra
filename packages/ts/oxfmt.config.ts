import { defineConfig } from "oxfmt";

export default defineConfig({
  printWidth: 100,
  // The generated standard WASI table is verified byte-for-byte by its Rust bless test. The root
  // `.prettierignore` also leaves out the manifest of the TOML 1.1 configuration fixture, whose
  // trailing comma the formatter would remove: these patterns do not reach it there.
  ignorePatterns: [
    "**/node_modules/**",
    "**/dist/**",
    "**/target/**",
    "**/*.lock",
    "**/sdk/src/interpreter/standard.ts",
  ],
});
