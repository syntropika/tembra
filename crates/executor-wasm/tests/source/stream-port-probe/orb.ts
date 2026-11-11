import { orb, type Context, type Declaration } from "@syntropika/tembra-sdk";

const MATH = "tembra-test:port-refusal/math@1.0.0";

function create(self: Declaration) {
  return { math: self.get<{ double(value: number): number }>(MATH, ["main"]) };
}
type Instance = ReturnType<typeof create>;

function message(error: unknown): string {
  return error instanceof Error ? `${error.name}: ${error.message}` : String(error);
}

/** Typed-import refusals a platform data port must survive without losing its revision. */
export default orb({
  name: "port-refusal",
  apiVersion: 1,
  services: {
    probe: {
      create,
      methods: {
        async forge(instance: Instance, _context: Context, which: number): Promise<string> {
          try {
            switch (which) {
              case 0: {
                // An argument the import's signature refuses, then a valid call.
                let refused = "accepted";
                try {
                  instance.math.double(300);
                } catch (error) {
                  refused = message(error);
                }
                return `${refused} | ${instance.math.double(21)}`;
              }
              case 1:
                // The platform refuses the selected reference (the embedding answers its status
                // check with an error).
                return `accepted ${instance.math.double(21)}`;
              default:
                throw new Error("unknown forgery");
            }
          } catch (error) {
            return message(error);
          }
        },
        ping(): number {
          return 42;
        },
      },
    },
  },
});
