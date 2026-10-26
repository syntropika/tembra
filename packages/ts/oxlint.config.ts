import { defineConfig } from "oxlint";

export default defineConfig({
  plugins: ["typescript", "import", "promise"],
  categories: { correctness: "error" },
  ignorePatterns: ["**/node_modules/**", "**/dist/**", "target/**"],
  rules: {
    "typescript/no-this-alias": "off",
    "promise/no-callback-in-promise": "off",
    "no-debugger": "error",
    "no-unused-vars": ["error", { argsIgnorePattern: "^_", ignoreRestSiblings: true }],
  },
});
