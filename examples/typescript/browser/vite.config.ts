import { defineConfig } from "vite";

export default defineConfig({
  root: import.meta.dirname,
  // Preserve the SDK module URLs used to locate its packaged wasm-bindgen assets.
  optimizeDeps: { exclude: ["@tembra/sdk", "@tembra/sdk/browser"] },
});
