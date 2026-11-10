import { orb, type Context, type Declaration } from "@syntropika/tembra-sdk";
import type { ExamplePrecisionNumbers100 as Numbers, relay } from "./contract.wit";

function create(self: Declaration) {
  return { numbers: self.get<typeof Numbers>("example:precision/numbers@1.0.0", ["main"]) };
}
type Environment = ReturnType<typeof create>;

/** Relays every call through the WIT import; the values never leave bigint in this heap. */
export default orb({
  name: "relay",
  apiVersion: 1,
  services: {
    relay: {
      create,
      methods: {
        add(environment: Environment, context: Context, a: bigint, b: bigint) {
          return environment.numbers.add(a, b, context);
        },
        echo(
          environment: Environment,
          context: Context,
          sample: relay.Sample,
          pick: relay.Pick,
          tuple: [bigint, string],
        ) {
          return environment.numbers.echo(sample, pick, tuple, context);
        },
      },
    },
  },
});
