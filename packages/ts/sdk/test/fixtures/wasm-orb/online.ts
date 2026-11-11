import { orb, type Context, type Declaration, type WitMethods } from "@syntropika/tembra-sdk";
import { fetch } from "@syntropika/tembra-sdk/http";
import type { network, ExampleIntegratedArithmetic100 as Arithmetic } from "./contract.wit";

function create(self: Declaration) {
  return {
    arithmetic: self.get<typeof Arithmetic>("example:integrated/arithmetic@1.0.0", ["main"]),
  };
}
type Environment = ReturnType<typeof create>;

/** Fetch a JSON list of numbers and double its sum through the arithmetic dependency. */
async function fetchSum(environment: Environment, context: Context, url: string) {
  const response = await fetch(url);
  if (!response.ok) {
    await response.close();
    throw new Error(`HTTP request failed: ${response.status}`);
  }
  const input = await response.json<{ values: number[] }>();
  const sum = input.values.reduce((total, value) => total + value, 0);
  return environment.arithmetic.double(sum, context);
}

export default orb({
  name: "online",
  apiVersion: 1,
  services: {
    network: {
      create,
      methods: {
        async fetchAndSum(environment: Environment, context: Context, url: string) {
          return fetchSum(environment, context, url);
        },
        // A thrown error reaches the declared `result` lane as its message (`string` err).
        async fetchAndSumChecked(environment: Environment, context: Context, url: string) {
          return fetchSum(environment, context, url);
        },
        async firstEvent(_environment: Environment, _context: Context, url: string) {
          const response = await fetch(url);
          for await (const event of response.events()) return event.data;
          throw new Error("SSE ended without an event");
        },
      } satisfies WitMethods<typeof network, Environment>,
    },
  },
});
