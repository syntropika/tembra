import { expect, test } from "bun:test";
import { readFile } from "node:fs/promises";
import { createContext, runInContext } from "node:vm";

/** Exercise the actual heap-side queue without serializing its internal envelopes. */
test("effect identities settle once, out of order, and pending effects reject at scope end", async () => {
  let active = true;
  const context = createContext({ __scopeActive: () => active });
  const source = await readFile(
    new URL("../../../../crates/executor-wasm/guest/src/bridge.ts", import.meta.url),
    "utf8",
  );
  runInContext(new Bun.Transpiler({ loader: "ts" }).transformSync(source), context);
  const first = context.__hostAsync("first");
  const second = context.__hostAsync("second");
  const requests = context.__tembraTakeEffects();
  expect(JSON.stringify(requests)).toBe('[{"id":1,"packet":"first"},{"id":2,"packet":"second"}]');
  expect(context.__tembraTakeEffects().length).toBe(0);
  context.__tembraCompleteEffect(2, true, "42");
  expect(await second).toBe("42");
  expect(() => context.__tembraCompleteEffect(2, true, "again")).toThrow("unknown or completed");
  expect(() => context.__tembraCompleteEffect(99, true, "foreign")).toThrow("unknown or completed");
  const cancelled = first.catch((error: Error) => error.message);
  context.__tembraStopEffects();
  expect(await cancelled).toBe("callback scope ended");
  expect(context.__tembraTakeEffects().length).toBe(0);
  active = false;
  expect(() => context.__hostAsync("late")).toThrow("callback scope is closed");
});
