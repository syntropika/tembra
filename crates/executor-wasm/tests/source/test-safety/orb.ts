import { orb, type Context, type Declaration, type State } from "@syntropika/tembra-sdk";

let saved: State<number> | undefined;
let savedContext: Context | undefined;
let deferredDenied = false;
// Module evaluation has no host: output scheduled from it, even deferred, is refused.
void Promise.resolve().then(() => {
  try {
    console.log("rogue");
  } catch {
    deferredDenied = true;
  }
});

function create(self: Declaration) {
  const state = self.state(1, { validate: (value) => value < 1000 });
  saved ??= state;
  return { state };
}
type Environment = ReturnType<typeof create>;

export default orb({
  name: "safe",
  apiVersion: 1,
  services: {
    checks: {
      create,
      methods: {
        async run(environment: Environment, context: Context, mode: string): Promise<number> {
          if (mode === "save-context") {
            savedContext = context;
            return 1;
          }
          if (mode === "stale-context") {
            try {
              savedContext!.check();
            } catch {
              return 2;
            }
            return 0;
          }
          if (mode === "deferred") return deferredDenied ? 1 : 0;
          if (mode === "cached") {
            // Another object's state handle, retained from its factory.
            try {
              return saved!.value;
            } catch {
              return 2;
            }
          }
          if (mode === "floating") {
            // Detached work finishes before the next call, but its context ended with this call.
            void (async () => {
              await tembra.wait(50);
              environment.state.update(context, () => 999);
            })().catch(() => {});
            return 1;
          }
          if (mode === "many") {
            const results = await Promise.allSettled(
              Array.from({ length: 256 }, () => tembra.wait(1)),
            );
            return results.length;
          }
          if (mode === "wait") {
            await tembra.wait(10);
            return 1;
          }
          if (mode === "long-wait") {
            await tembra.wait(80);
            return 1;
          }
          if (mode === "state-checks") {
            const before = environment.state.value;
            for (const invalid of [Number.NaN, undefined, new Date(0), 1001, -0]) {
              let rejected = false;
              try {
                environment.state.update(context, () => invalid as number);
              } catch {
                rejected = true;
              }
              if (!rejected || environment.state.value !== before)
                throw new Error("Invalid state was adopted");
            }
            let computations = 0;
            const computed = () => computations;
            const selected = environment.state.select((value) => {
              computations++;
              return { doubled: value * 2 };
            });
            const first = selected.value;
            if (first !== selected.value || !Object.isFrozen(first) || computed() !== 1)
              throw new Error("Readable identity changed");
            environment.state.update(context, (value) => value + 1);
            if (selected.value.doubled !== 4 || computed() !== 2 || first.doubled !== 2)
              throw new Error("Readable snapshot changed");
            environment.state.update(context, () => before);
            return 1;
          }
          // A promise no host operation settles: the runtime refuses the call that left it.
          if (mode === "hang") await new Promise(() => {});
          if (mode === "cpu") while (true) {}
          return environment.state.value;
        },
      },
    },
  },
});
