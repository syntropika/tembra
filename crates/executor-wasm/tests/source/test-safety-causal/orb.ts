import { orb, type Context, type Declaration, type WitMethods } from "@syntropika/tembra-sdk";
import type { checks } from "./contract.wit";

function create(self: Declaration) {
  return {
    state: self.state(1),
    // A `result` import returns its `ok` value or throws its `err`.
    bridge: self.get<{ bounce(): Promise<number> }>("example:safety-causal/bridge"),
  };
}
type Environment = ReturnType<typeof create>;

export default orb({
  name: "safe",
  apiVersion: 1,
  services: {
    checks: {
      create,
      methods: {
        // A refused bounce is the bridge's WIT `result` error; it is rethrown as this call's.
        async reenter(environment: Environment, _context: Context) {
          return await environment.bridge.bounce();
        },
        read(environment: Environment) {
          return environment.state.value;
        },
      } satisfies WitMethods<typeof checks, Environment>,
    },
  },
});
