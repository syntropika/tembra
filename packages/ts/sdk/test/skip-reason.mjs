// The skip reason of a compiled-Orb test: `skip` takes it, and `skip-reporter.mjs` counts it.
export const skipPrefix = "set TEMBRA_WASM_";

// False when the variable is set, else the text node:test prints and the reporter counts.
export function missing(name) {
  return process.env[name] ? false : `${skipPrefix}${name.slice("TEMBRA_WASM_".length)}`;
}
