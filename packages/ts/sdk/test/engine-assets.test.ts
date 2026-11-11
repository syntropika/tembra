import { expect, test } from "bun:test";
import { interpreterURL } from "@syntropika/tembra-engine-wasm/assets";

test("the engine package locates the interpreter named by its raw export", async () => {
  // Unit jobs build the locator without building the interpreter; packaging tests compile the asset.
  const packageRoot = new URL("../../engine-wasm/", import.meta.url);
  const manifest: { exports: { "./interpreter": string } } = await Bun.file(
    new URL("package.json", packageRoot),
  ).json();
  const expected = new URL(manifest.exports["./interpreter"], packageRoot).href;
  const located = interpreterURL();
  expect(located.href).toBe(expected);
  // Each caller receives its own URL, so a caller cannot redirect later engine loads.
  expect(interpreterURL()).not.toBe(located);
  located.pathname = "/changed.wasm";
  expect(interpreterURL().href).toBe(expected);
});
