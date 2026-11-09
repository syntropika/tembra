/** Return a new URL for the interpreter core module shipped next to this module. */
export function interpreterURL(): URL {
  // Keep the literal form so asset-aware bundlers can discover the Wasm file.
  return new URL("./interpreter/interpreter.wasm", import.meta.url);
}
