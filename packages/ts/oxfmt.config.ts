import { defineConfig } from "oxfmt";

export default defineConfig({
  printWidth: 100,
  // The generated standard WASI table is verified byte-for-byte by its Rust bless test.
  ignorePatterns: [
    "**/node_modules/**",
    "**/dist/**",
    "**/target/**",
    "**/*.lock",
    "**/sdk/src/interpreter/standard.ts",
  ],
});
