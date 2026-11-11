import { orb } from "@syntropika/tembra-sdk";
import type { calculation, ExampleIntegratedArithmetic100 as Arithmetic } from "./contract.wit";
import type { Context, Declaration, WitMethods } from "@syntropika/tembra-sdk";
const sum = (values: readonly number[]) => values.reduce((total, value) => total + value, 0);

// Module evaluation runs at build time without host authority: console output is refused here.

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
          // The component runtime lifts list<s32> as an array of numbers.
          if (!Array.isArray(input.values)) throw new Error("WIT list shape differs");
          const total = sum([...input.values]);
          // `double` is a synchronous WIT function shared with the compiled Rust provider: the
          // typed import blocks this callback and returns its value.
          const doubled: number = environment.arithmetic.double(total, context);
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
        // The old private host protocol no longer exists; probing it fails the call.
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
          // The marker lets a caller cancel only after the wait was really entered.
          console.info("pending: waiting");
          await tembra.wait(5000);
          return 1;
        },
      } satisfies WitMethods<typeof calculation, Environment>,
    },
  },
});
