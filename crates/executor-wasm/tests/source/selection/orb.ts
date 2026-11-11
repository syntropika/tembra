import { orb, type Context, type Declaration } from "@syntropika/tembra-sdk";

const MATH = "tembra-test:selection/math@1.0.0";

function create(self: Declaration) {
  return { math: self.get<{ double(value: number): number }>(MATH) };
}
type Instance = ReturnType<typeof create>;

/** A recoverable argument error before the host call must not poison the next dependency call. */
export default orb({
  name: "selection",
  apiVersion: 1,
  services: {
    probe: {
      create,
      methods: {
        async recover(instance: Instance, _context: Context) {
          let refused = "accepted";
          try {
            instance.math.double(300);
          } catch (error) {
            refused = (error as Error).name;
          }
          return `${refused} ${instance.math.double(21)}`;
        },
      },
    },
  },
});
