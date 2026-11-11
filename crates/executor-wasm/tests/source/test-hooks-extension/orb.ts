import { orb, type Context, type Declaration, type WitMethods } from "@syntropika/tembra-sdk";
import type { guard, reporter } from "./contract.wit";

/**
 * Managed state belongs to one service object, so the Orb root keeps its observations in a
 * root-owned resource; the reporter service reads that resource through `Declaration.orb`.
 */
function create(self: Declaration) {
  return { counts: self.resource("counts", { open: () => ({ audit: 0, choices: 0 }) }) };
}
type Root = ReturnType<typeof create>;

export default orb({
  name: "extension",
  apiVersion: 1,
  create,
  contributes: {
    "example:hooks/guard@1.0.0": {
      priority: 0,
      handle(_root: Root, context: Context, input: guard.Admission): guard.Admission {
        context.check();
        return { value: input.value + 2, blocked: true };
      },
    },
    "example:hooks/audit@1.0.0": {
      priority: 0,
      handle(root: Root, context: Context, input: number): void {
        context.check();
        root.counts.value.audit += input;
      },
    },
    "example:hooks/choice@1.0.0": {
      priority: 0,
      // The compiled contributor decides input 1 first, so 11 is never observed with it installed.
      handle(root: Root, context: Context, input: number): number | undefined {
        context.check();
        root.counts.value.choices += 1;
        return input === 2 ? 84 : input === 1 ? 11 : undefined;
      },
    },
  },
  services: {
    reporter: {
      create(self: Declaration) {
        return self.orb as Root;
      },
      methods: {
        read(root) {
          const { audit, choices } = root.counts.value;
          return { audit, choices };
        },
      } satisfies WitMethods<typeof reporter, Root>,
    },
  },
});
