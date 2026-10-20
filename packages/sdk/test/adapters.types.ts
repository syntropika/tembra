import { Type, contractAdapter, local, method } from "../src/index.js";
import type { ContractAdapter, FacetDeclaration, ServeOptions } from "../src/index.js";

const Legacy = local("typed.adapter", {
  version: 1,
  input: Type.Object({ name: Type.String() }),
  members: { increment: method(Type.Integer(), Type.Integer()) },
});
const Current = local("typed.adapter", {
  version: 2,
  input: Type.Object({ name: Type.String() }),
  members: { add: method(Type.Integer(), Type.Integer()) },
});
const Other = local("typed.adapter", {
  version: 3,
  input: Type.Object({ count: Type.Integer() }),
  members: { multiply: method(Type.Integer(), Type.Integer()) },
});

function typeChecks(declaration: FacetDeclaration) {
  const legacy = contractAdapter(Current, Legacy, (current) => ({
    methods: { increment: current.methods.add },
    state: {},
  }));
  const options: ServeOptions<typeof Current> = { accepts: [legacy] };
  declaration.serve(
    Current,
    (input) => {
      const name: string = input.name;
      void name;
      return {
        methods: {
          add: (ctx, by) => {
            ctx.check();
            return by + 1;
          },
        },
        state: {},
      };
    },
    options,
  );
  // @ts-expect-error: adapter declarations are opaque, not caller-built backend records
  const forged: ContractAdapter<typeof Current> = { provider: "", consumer: "", adapt: () => ({}) };
  // @ts-expect-error: callers cannot inspect the private backend metadata through the public type
  void legacy.provider;
  // oxlint-disable typescript/require-await -- The negative fixture must return an actual Promise from an async adapter.
  // @ts-expect-error: synchronous adapters cannot return a Promise
  contractAdapter(Current, Legacy, async (current) => ({
    methods: { increment: current.methods.add },
    state: {},
  }));
  // oxlint-enable typescript/require-await
  contractAdapter(Current, Legacy, () => ({
    methods: {
      // @ts-expect-error: the consumer result schema is not widened by callback inference
      increment: (_ctx, by) => String(by),
    },
    state: {},
  }));
  contractAdapter(Current, Legacy, (current) => ({
    methods: {
      // @ts-expect-error: provider callbacks use the provider token's member names
      increment: current.methods.missing, // oxlint-disable-line typescript/no-unsafe-assignment -- Compile-only negative fixture deliberately selects a missing provider member.
    },
    state: {},
  }));
  // @ts-expect-error: accepting a different provider cannot widen the actual served contract
  declaration.serve(Other, () => ({ methods: { multiply: (_ctx, by) => by }, state: {} }), options);
  declaration.serve(
    Current,
    // @ts-expect-error: factory input is inferred only from the served provider token
    (input: { count: number }) => ({
      methods: { add: (_ctx, by) => by + input.count },
      state: {},
    }),
    { accepts: [legacy] },
  );
  void forged;
}
void typeChecks;
