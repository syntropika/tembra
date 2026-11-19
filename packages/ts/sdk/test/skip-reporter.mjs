// Prints one line after the run when compiled-Orb tests were skipped for want of a TEMBRA_WASM_*
// variable; `just test-wasm-acceptance` sets them all, so it prints nothing there.
import { skipPrefix } from "./skip-reason.mjs";

export default async function* (source) {
  let skipped = 0;
  for await (const event of source) {
    if (
      (event.type === "test:pass" || event.type === "test:fail") &&
      typeof event.data.skip === "string" &&
      event.data.skip.startsWith(skipPrefix)
    ) {
      skipped += 1;
    }
  }
  if (skipped > 0) {
    yield `\nSkipped ${skipped} compiled-Orb tests: their TEMBRA_WASM_* variables are unset. Run them with \`just test-wasm-acceptance\`.\n`;
  }
}
