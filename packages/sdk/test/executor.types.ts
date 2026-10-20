import {
  Type,
  defineFacet,
  hostNamed,
  local,
  method,
  native,
  opaque,
  remote,
} from "../src/index.js";
import type {
  Cell,
  ExplicitCapability,
  FacetDeclaration,
  FacetScope,
  ImplementationOf,
  ManagedContext,
  ObjectDeclaration,
  Readable,
} from "../src/index.js";

const Counter = local("executor.typed", {
  version: 1,
  input: Type.Object({ name: Type.String() }),
  members: {
    count: Type.Integer(),
    add: method(Type.Object({ by: Type.Integer() }), Type.Integer()),
    read: method(undefined, Type.Integer()),
    touch: method(),
    nullable: method(Type.Null(), Type.Null()),
    echo: method(native<URL>("url@1"), native<URL>("url@1")),
    capability: opaque<URL>("url@1"),
  },
});
const Remote = remote("executor.variant", {
  version: 1,
  input: Type.Object({}),
  host: hostNamed("service"),
  variants: {
    reader: { methods: { read: method(undefined, Type.Integer()) } },
    editor: {
      state: { count: Type.Integer() },
      methods: { add: method(Type.Integer(), Type.Integer()) },
    },
  },
});

async function typeChecks(
  declaration: FacetDeclaration,
  self: ObjectDeclaration,
  scope: FacetScope,
  context: ManagedContext,
  capability: ExplicitCapability<URL>,
) {
  const count = self.cell("count", { schema: Type.Integer(), initial: 0 });
  const checked: Cell<number> = count;
  checked.replace(context, 2, { diff: true });
  checked.change(context, (candidate) => {
    candidate.value += 1;
  });
  // @ts-expect-error: the schema determines the value type
  checked.replace(context, "wrong");
  self.cell("async", {
    schema: Type.Integer(),
    initialize: async (ctx) => {
      ctx.check();
      const dependency = await ctx.get(Counter, { name: "dependency" });
      return await dependency.read(ctx);
    },
  });
  // @ts-expect-error: a cell has exactly one initial-value strategy
  self.cell("both", { schema: Type.Integer(), initial: 1, initialize: () => 2 });
  // @ts-expect-error: a wrong initializer result is rejected
  self.cell("wrong", { schema: Type.Integer(), initialize: async () => "wrong" }); // oxlint-disable-line typescript/require-await -- Negative compile fixture must keep its asynchronous callback.
  // @ts-expect-error: draft callbacks cannot be async
  count.change(context, async () => {
    /* Fixture callback has no runtime effects. */
  });
  const double = self.derive([count] as const, Type.Integer(), ([value]) => value.value * 2);
  const numberReadable: Readable<number> = double;
  // @ts-expect-error: derive returns the schema's value, not a Promise
  self.derive([count], Type.Integer(), async () => 1); // oxlint-disable-line typescript/require-await -- Negative compile fixture must keep its asynchronous callback.
  const resource = self.resource("url", {
    deps: [self.get(Counter, { name: "dependency" })],
    open: (signal) => {
      // oxlint-disable-next-line typescript/no-floating-promises -- Compile-only access checks the notification property without changing synchronous open behavior.
      void signal.cancelled;
      return new URL("https://example.com/");
    },
    close: (url) => {
      void url.hostname;
    },
  });
  const url: URL = resource.value;
  const resourceCapability: ExplicitCapability<URL> = resource;
  // @ts-expect-error: runtime resource capability preserves its declared value type
  const wrongCapability: ExplicitCapability<string> = resource;
  void [resourceCapability, wrongCapability];
  // oxlint-disable typescript/no-unsafe-call, typescript/no-unsafe-return -- This negative compile fixture intentionally names an unavailable operation.
  // @ts-expect-error: resource open receives notification, not Context authority
  self.resource("bad", { open: (signal) => signal.get(Counter, { name: "wrong" }) });
  // oxlint-enable typescript/no-unsafe-call, typescript/no-unsafe-return
  // @ts-expect-error: declarations cannot override principal
  self.get(Counter, { name: "dependency" }, { principal: "forged" });
  // @ts-expect-error: typed acquisition validates input statically
  self.get(Counter, { name: 3 });
  declaration.serve(
    Counter,
    (input, object) => {
      const name: string = input.name;
      void name;
      return {
        methods: {
          add(ctx, args) {
            ctx.check();
            return args.by;
          },
          read: () => 1,
          touch: () => undefined,
          nullable: (_ctx, value) => value,
          echo: (_ctx, value) => value,
        },
        state: { count: object.cell("count", { schema: Type.Integer(), initial: 0 }) },
        opaque: { capability },
      };
    },
    { lifetime: "host" },
  );
  // @ts-expect-error: factories are synchronous
  declaration.serve(Counter, async () => ({})); // oxlint-disable-line typescript/require-await -- Negative compile fixture must keep its asynchronous callback.
  const wrong: ImplementationOf<typeof Counter> = {
    methods: {
      // @ts-expect-error: result type comes from the method schema
      add: (_ctx, args) => String(args.by),
      read: () => 1,
      touch: () => undefined,
      // @ts-expect-error: null and absence are distinct
      nullable: () => undefined,
      // @ts-expect-error: native nominal type retains URL
      echo: (_ctx, value: string) => value,
    },
    state: { count },
    // @ts-expect-error: an ordinary value is not a minted opaque capability
    opaque: { capability: url },
  };
  const reader: ImplementationOf<typeof Remote> = {
    variant: "reader",
    methods: { read: () => 2 },
    state: {},
  };
  const editor: ImplementationOf<typeof Remote> = {
    variant: "editor",
    methods: { add: (_ctx, by) => by + 1 },
    state: { count },
  };
  declaration.serve(Remote, () => ({
    variant: "editor",
    methods: {
      add(ctx, by) {
        ctx.check();
        return by + 1;
      },
    },
    state: { count },
  }));
  const mismatch: ImplementationOf<typeof Remote> = {
    variant: "editor",
    // @ts-expect-error: required members follow the selected variant
    methods: { read: () => 2 },
    state: {},
  };
  const facet = defineFacet({
    name: "typed.facet",
    uses: [Counter, Remote],
    setup(api, declare) {
      void api.signal;
      void declare;
    },
  });
  const literalName: "typed.facet" = facet.name;
  const first: typeof Counter = facet.uses[0];
  // oxlint-disable typescript/no-unsafe-call -- This negative compile fixture intentionally names an unavailable operation.
  // @ts-expect-error: copied metadata is readonly even when the input array is mutable
  facet.uses.push(Counter);
  // oxlint-enable typescript/no-unsafe-call
  const mutableUses = [Counter];
  const copied = defineFacet({
    name: "copied",
    uses: mutableUses,
    setup() {
      /* Fixture callback has no runtime effects. */
    },
  });
  // oxlint-disable typescript/no-unsafe-call -- This negative compile fixture intentionally names an unavailable operation.
  // @ts-expect-error: a mutable caller-owned array does not weaken returned metadata
  copied.uses.push(Counter);
  // oxlint-enable typescript/no-unsafe-call
  defineFacet({
    name: "async.facet",
    uses: [],
    // @ts-expect-error: setup must return synchronously
    setup: async () => {
      /* Fixture callback has no runtime effects. */
    },
  });
  await scope.root(async (ctx) => {
    const ref = await ctx.get(Counter, { name: "root" });
    return await ref.read(ctx);
  });
  void [numberReadable, wrong, reader, editor, mismatch, literalName, first];
}
void typeChecks;
