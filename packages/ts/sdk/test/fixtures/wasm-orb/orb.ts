import { orb } from "@syntropika/tembra-sdk";
import type { calculation, ExampleIntegratedArithmetic100 as Arithmetic } from "./contract.wit";
import type { Context, Declaration, WitMethods } from "@syntropika/tembra-sdk";
const sum = (values: readonly number[]) => values.reduce((total, value) => total + value, 0);
import { fetch } from "@syntropika/tembra-sdk/http";

console.log("module log");

/** Create a managed environment; executable callbacks stay in the isolated guest. */
function create(self: Declaration) {
  console.info("factory log");
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
        checked(_environment: Environment, _context: Context, value: number) {
          if (value < 0) throw Object.assign(new Error("Rejected input"), { payload: "negative" });
          return value;
        },
        async sum(environment: Environment, context: Context, input: calculation.Input) {
          if (!(input.values instanceof Int32Array)) throw new Error("WIT list shape differs");
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
        fail() {
          throw new Error("Expected guest failure");
        },
        recover() {
          console.warn("method log");
          return 42;
        },
        forbidden() {
          return (globalThis as unknown as { __hostSync(packet: string): number }).__hostSync(
            JSON.stringify({ operation: "state.declare", initial: 999, version: 1 }),
          );
        },
        malformed() {
          return (globalThis as unknown as { __hostSync(packet: string): number }).__hostSync(
            '{"operation":"unknown"}',
          );
        },
        async race(environment: Environment, context: Context) {
          return Promise.race([
            environment.arithmetic.double(6, context),
            tembra.wait(1).then(() => -1),
          ]);
        },
        async fanout(environment: Environment, context: Context) {
          const values = await Promise.all(
            [3, 4, 5].map((value) => environment.arithmetic.double(value, context)),
          );
          return sum(values);
        },
        async pending() {
          await tembra.wait(5000);
          return 1;
        },
        async fetchAndSum(environment: Environment, context: Context, url: string) {
          const response = await fetch(url);
          if (!response.ok) throw new Error(`HTTP request failed: ${response.status}`);
          const input = await response.json<{ values: number[] }>();
          const doubled: number = await environment.arithmetic.double(sum(input.values), context);
          environment.total.update(context, () => doubled);
          return environment.total.value;
        },
        async firstEvent(_environment: Environment, _context: Context, url: string) {
          const response = await fetch(url);
          for await (const event of response.events()) return event.data;
          throw new Error("SSE ended without an event");
        },
      } satisfies WitMethods<typeof calculation, Environment>,
    },
  },
});
