import { Type, defineFacet, hostNamed, local, method, remote } from "@tembra/sdk";

const Counter = remote("test.worker.manager.counter", {
  version: 1,
  input: Type.Object({}),
  host: hostNamed("managed-worker"),
  variants: {
    main: {
      state: { count: Type.Integer() },
      methods: { increment: method(undefined, Type.Integer()), hang: method(), crash: method() },
    },
  },
});
const Flush = remote("test.worker.manager.parent-flush", {
  version: 1,
  input: Type.Object({}),
  host: hostNamed("worker-manager-parent"),
  variants: { main: { methods: { flush: method() } } },
});
const Owner = local("test.worker.manager.eager-owner", {
  version: 1,
  input: Type.Object({}),
  members: {},
});
export default defineFacet({
  name: "worker-manager-counter",
  uses: [Flush],
  setup(scope, declarations) {
    declarations.serve(
      Owner,
      (_input, self) => {
        const parent = self.get(Flush, {});
        self.resource("flush-on-close", {
          deps: [parent],
          open: () => ({}),
          async close() {
            await scope.root(async (context) => {
              await parent.flush(context);
            });
          },
        });
        return { state: {}, methods: {} };
      },
      { lifetime: "host" },
    );
    declarations.serve(Counter, (_input, self) => {
      const count = self.cell("count", { schema: Type.Integer(), initial: 0 });
      return {
        variant: "main",
        state: { count },
        methods: {
          increment: (ctx) => count.replace(ctx, count.value + 1).value,
          async hang(ctx) {
            await ctx.signal.cancelled;
          },
          crash() {
            setTimeout(() => {
              throw new Error("managed Worker fault");
            }, 10);
            return undefined;
          },
        },
      };
    });
  },
});
