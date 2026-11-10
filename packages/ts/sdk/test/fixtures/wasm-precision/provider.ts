import { orb, type Context } from "@syntropika/tembra-sdk";
import type { numbers } from "./contract.wit";

/** Exact bigint arithmetic; a result outside u64 is refused by the guest lowering. */
export default orb({
  name: "precision",
  apiVersion: 1,
  services: {
    numbers: {
      methods: {
        add(_environment: unknown, _context: Context, a: bigint, b: bigint) {
          return a + b;
        },
        echo(
          _environment: unknown,
          _context: Context,
          sample: numbers.Sample,
          pick: numbers.Pick,
          tuple: [bigint, string],
        ) {
          // A negative signed pick is the declared `err` case, carried as an s64 payload.
          if (pick.tag === "signed" && pick.val < 0n)
            throw Object.assign(new Error("negative pick"), { payload: pick.val });
          return {
            ...sample,
            id: tuple[0],
            values: BigInt64Array.from([...sample.values].reverse()),
            maybe: pick.tag === "signed" ? BigInt.asUintN(64, pick.val) : sample.maybe,
            label: `${sample.label}:${tuple[1]}`,
          };
        },
      },
    },
  },
});
