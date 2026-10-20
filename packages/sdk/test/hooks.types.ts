import { Type, defineHook, hookHandler, local, native } from "../src/index.js";
import type { ImplementationOf } from "../src/index.js";

const integer = Type.Integer();
const middleware = defineHook("typed.middleware", {
  version: 1,
  strategy: "middleware",
  input: integer,
  result: Type.String(),
  terminal: { input: integer, result: Type.String() },
});
const Middleware = local("typed.contribution", {
  version: 1,
  input: Type.Object({}),
  members: { handle: middleware.method },
});
const implementation: ImplementationOf<typeof Middleware> = {
  state: {},
  methods: {
    handle: hookHandler(middleware, async (context, args) => {
      context.check();
      const input: number = args.input;
      try {
        return await args.next(input + 1);
      } catch {
        return await args.next.recover("fallback");
      }
    }),
  },
};
void implementation;
// @ts-expect-error: wrong recovery type cannot widen the selected hook
hookHandler(middleware, async (_context, args) => await args.next.recover(4));
// @ts-expect-error: wrong handler output cannot widen the selected hook
hookHandler(middleware, () => 123);
// @ts-expect-error: middleware requires its terminal profile
defineHook("bad", { version: 1, strategy: "middleware", input: integer, result: integer });
const series = defineHook("series", { version: 1, strategy: "series", input: integer });
// oxlint-disable typescript/no-unsafe-call, typescript/no-unsafe-return -- The negative type fixture deliberately accesses a missing series continuation.
// @ts-expect-error: series has no continuation
hookHandler(series, (_context, args) => args.next(args.input));
// oxlint-enable typescript/no-unsafe-call, typescript/no-unsafe-return
// @ts-expect-error: series has exact unit output, not TypeScript void
hookHandler(series, () => 123);
const waterfall = defineHook("waterfall", { version: 1, strategy: "waterfall", input: integer });
// @ts-expect-error: waterfall returns the accumulator
hookHandler(waterfall, () => "wrong");
const bail = defineHook("bail", {
  version: 1,
  strategy: "bail",
  result: Type.Union([Type.Boolean(), Type.Null()]),
});
hookHandler(bail, () => ({ kind: "stop", value: false }));
hookHandler(bail, () => ({ kind: "stop", value: null }));
// @ts-expect-error: false is not an implicit bail control result
hookHandler(bail, () => false);
const marker = native<{ marker: true }>("typed.marker@1");
defineHook("parallel", {
  version: 1,
  strategy: "parallel",
  result: marker,
  terminal: { input: native<readonly { marker: true }[]>("typed.vector@1"), result: undefined },
});
defineHook("parallel", {
  version: 1,
  strategy: "parallel",
  result: marker,
  // @ts-expect-error: native results cannot become an automatic portable array
  terminal: { input: Type.Array(Type.Unknown()), result: undefined },
});
