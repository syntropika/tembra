import { expect, test } from "bun:test";
import { interpreterURL } from "@syntropika/tembra-engine-wasm/assets";

test("the engine package locates the interpreter named by its raw export", () => {
  const located = interpreterURL();
  expect(located.href).toBe(import.meta.resolve("@syntropika/tembra-engine-wasm/interpreter"));
  // Each caller receives its own URL, so a caller cannot redirect later engine loads.
  expect(interpreterURL()).not.toBe(located);
});
