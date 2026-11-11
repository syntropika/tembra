import { orb, type Context, type Declaration } from "@syntropika/tembra-sdk";

// The first revision.
function create(self: Declaration) {
  const total = self.state(0, { version: 1 });
  return { total };
}
type Instance = ReturnType<typeof create>;

export default orb({
  name: "ledger",
  apiVersion: 1,
  services: {
    ledger: {
      create,
      methods: {
        add(instance: Instance, context: Context, amount: number) {
          return instance.total.update(context, (current) => current + amount);
        },
        revision() {
          return 1;
        },
      },
    },
  },
});
