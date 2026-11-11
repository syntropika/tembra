import {
  bail,
  orb,
  series,
  waterfall,
  type Declaration,
  type WitMethods,
} from "@syntropika/tembra-sdk";
import type { guard, runner } from "./contract.wit";

/** Bind the provider's three hooks; the kernel supplies their versioned participant plans. */
function create(self: Declaration) {
  return {
    guard: self.hook<guard.Admission>("guard"),
    audit: self.hook<number, undefined>("audit"),
    choice: self.hook<number, number | undefined>("choice"),
  };
}

export default orb({
  name: "tools",
  apiVersion: 1,
  hooks: { guard: waterfall(), audit: series(), choice: bail() },
  services: {
    runner: {
      create,
      methods: {
        run(hooks, context, input) {
          return hooks.guard.run(context, input);
        },
        audit(hooks, context, input) {
          return hooks.audit.run(context, input);
        },
        choose(hooks, context, input) {
          return hooks.choice.run(context, input);
        },
      } satisfies WitMethods<typeof runner, ReturnType<typeof create>>,
    },
  },
});
