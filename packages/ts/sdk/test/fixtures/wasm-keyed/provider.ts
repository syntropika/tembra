import { orb, type Context, type Declaration } from "@syntropika/tembra-sdk";

function create(self: Declaration) {
  // The `slow` instance stays unready long enough for its consumer to be cancelled.
  if (self.key[0] === "slow")
    self.resource("connection", {
      async open() {
        await tembra.wait(3000);
        return 1;
      },
    });
  const name = `${self.key.join("/")}@${self.principal}`;
  // Lets hosts assert which instances were activated, for example none for a cleanup principal.
  console.info(`activate ${name}`);
  return { name, count: 0 };
}
type Instance = ReturnType<typeof create>;

/** Keyed counters reporting the kernel address each instance serves. */
export default orb({
  name: "keyed",
  apiVersion: 1,
  services: {
    counter: {
      create,
      methods: {
        add(instance: Instance, _context: Context, amount: number) {
          instance.count += amount;
          return instance.count;
        },
        who(instance: Instance) {
          return instance.name;
        },
      },
    },
  },
});
