import { orb } from "@syntropika/tembra-sdk";
import type { calculation, ExampleIntegratedArithmetic100 as Arithmetic } from "./contract.wit";
import type { Context, Declaration, WitMethods } from "@syntropika/tembra-sdk";
import { sum } from "simple-statistics";
import { fetch } from "@syntropika/tembra-sdk/http";

/** Create a managed environment; executable callbacks stay in the isolated guest. */
function create(self: Declaration) {
  const total = self.state(0, { version: 1 });
  const arithmetic = self.get<typeof Arithmetic>("example:integrated/arithmetic@1.0.0", ["main"]);
  const resource = self.resource("number", {
    open: () => 21,
    close: () => {},
  });
  const doubled = self.resource("doubled-number", {
    deps: [resource],
    open: () => resource.value * 2,
    close: () => {},
  });
  return { total, arithmetic, resource: doubled };
}

type Environment = ReturnType<typeof create>;

export default orb({
  name: "calculator",
  apiVersion: 1,
  services: {
    calculation: {
      create,
      methods: {
        async sum(environment: Environment, context: Context, input: calculation.Input) {
          const total = sum([...input.values]);
          const doubled: number = await environment.arithmetic.double(total, context);
          environment.total.update(context, () => doubled);
          return environment.total.value;
        },
        read(environment: Environment) {
          return environment.total.value;
        },
        resourceValue(environment: Environment) {
          return environment.resource.value;
        },
        async fetchAndSum(environment: Environment, context: Context, url: string) {
          const response = await fetch(url);
          if (!response.ok) throw new Error(`HTTP request failed: ${response.status}`);
          const input = await response.json<{ values: number[] }>();
          const doubled: number = await environment.arithmetic.double(sum(input.values), context);
          environment.total.update(context, () => doubled);
          return environment.total.value;
        },
      } satisfies WitMethods<typeof calculation, Environment>,
    },
  },
});
