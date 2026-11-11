import { orb, waterfall, type Declaration, type WitMethods } from "@syntropika/tembra-sdk";
import type { runner, tally } from "./contract.wit";

/** The provider runs its own waterfall hook through the hook's typed WIT function. */
function create(self: Declaration) {
  return self.hook<tally.Entry>("tally");
}

export default orb({
  name: "tools",
  apiVersion: 1,
  hooks: { tally: waterfall() },
  services: {
    runner: {
      create,
      methods: {
        run(hook, context, input) {
          return hook.run(context, input);
        },
      } satisfies WitMethods<typeof runner, ReturnType<typeof create>>,
    },
  },
});
