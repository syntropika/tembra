import { orb, type Context, type Declaration } from "@syntropika/tembra-sdk";
import type { ExampleKeyedCounter100 as Counter } from "./contract.wit";

const COUNTER = "example:keyed/counter@1.0.0";
// Deliberately retained past their callback and root, to prove the host refuses them later.
let retainedContext: Context | undefined;
let retainedCounter: { add(amount: number): number } | undefined;

/** Report whether an operation was accepted or the message it was refused with. */
async function outcome(operation: () => unknown): Promise<string> {
  try {
    await operation();
    return "accepted";
  } catch (error) {
    return `refused: ${(error as Error).message}`;
  }
}

/**
 * A resource close runs under a cleanup context, without an admitted invocation: acquiring there
 * would select instances by the cleanup principal, so the host refuses it. The outcome is logged
 * because nothing awaits a close.
 */
function create(self: Declaration) {
  self.resource("session", {
    open: () => 1,
    async close(_value: number, context: Context) {
      const result = await outcome(() => context.get<typeof Counter>(COUNTER, "closing"));
      console.info(`close-acquire ${result}`);
    },
  });
  return {};
}

/** Selects counter instances by key at runtime, under the caller's root and principal. */
export default orb({
  name: "router",
  apiVersion: 1,
  services: {
    router: {
      create,
      methods: {
        async route(
          _environment: unknown,
          context: Context,
          key: string,
          other: string,
          amount: number,
        ) {
          const first = await context.get<typeof Counter>(COUNTER, key);
          const again = await context.get<typeof Counter>(COUNTER, [key]);
          const second = await context.get<typeof Counter>(COUNTER, other);
          const counts = [
            await first.add(amount, context),
            await again.add(amount, context),
            await second.add(amount),
          ];
          retainedContext = context;
          retainedCounter = first;
          return `${counts.join(",")} ${await again.who()} ${await second.who(context)}`;
        },
        async probe(_environment: unknown, context: Context, name: string) {
          switch (name) {
            case "retained-context":
              return outcome(() => retainedContext!.get<typeof Counter>(COUNTER, "a"));
            case "retained-reference":
              return outcome(() => retainedCounter!.add(1));
            case "unknown-import":
              return outcome(() => context.get("example:keyed/router@1.0.0", "a"));
            case "key-kind":
              return outcome(() => context.get(COUNTER, [1] as never));
            case "release": {
              const counter = await context.get<typeof Counter>(COUNTER, "released");
              counter.release();
              counter.release();
              return counter.status;
            }
            case "slow":
              return outcome(() => context.get<typeof Counter>(COUNTER, "slow"));
            default:
              throw new Error(`Unknown probe ${name}`);
          }
        },
      },
    },
  },
});
