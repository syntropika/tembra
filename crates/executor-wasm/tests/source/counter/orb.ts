import { orb, type Context, type Declaration } from "@syntropika/tembra-sdk";

/** One exact u64 total per instance, kept as decimal text in kernel state. */
function create(self: Declaration) {
  return { total: self.state("0", { version: 1 }) };
}
type Instance = ReturnType<typeof create>;

export default orb({
  name: "counter",
  apiVersion: 1,
  services: {
    counter: {
      create,
      methods: {
        add(instance: Instance, context: Context, amount: bigint) {
          const next = BigInt(instance.total.value) + amount;
          console.info(`add ${amount}`);
          instance.total.update(context, () => next.toString());
          return next;
        },
        whoami(_instance: Instance, context: Context) {
          return context.principal;
        },
      },
    },
  },
});
