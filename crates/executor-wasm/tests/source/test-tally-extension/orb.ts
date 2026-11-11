import { orb, type Context } from "@syntropika/tembra-sdk";
import type { tally } from "./contract.wit";

export default orb({
  name: "extension",
  apiVersion: 1,
  contributes: {
    "example:tally/tally@1.0.0": {
      priority: 0,
      // Refuse anything but `bigint` integers, so a lossy JavaScript number cannot pass silently.
      // Below zero, the exact subtraction yields -1n, which the canonical `u64` lowering refuses.
      handle(_root: unknown, context: Context, input: tally.Entry): tally.Entry {
        context.check();
        if (typeof input.total !== "bigint" || typeof input.delta !== "bigint")
          throw new Error("a 64-bit integer did not arrive as a bigint");
        return { total: input.total - 1n, delta: input.delta + 1n, note: `${input.note}+source` };
      },
    },
  },
});
