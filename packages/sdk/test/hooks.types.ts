import { Type, defineHook, hookFromDescriptor, hookHandler, local, native } from "../src/index.js";
import type {
  ImplementationOf,
  FacetScope,
  ManagedContext,
  RegisteredOperation,
} from "../src/index.js";

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

/** Compile-only checks for strategy-specific run results and terminal admission. */
export async function hookRunTypes(
  scope: FacetScope,
  context: ManagedContext,
  terminal: RegisteredOperation,
): Promise<unknown> {
  const result: string = await scope.runHook(middleware, context, 1, { terminal });
  // oxlint-disable-next-line typescript/no-confusing-void-expression -- The compile-only fixture verifies the exact absent output type.
  const unit: undefined = await scope.runHook(series, context, 1);
  const stop = await scope.runHook(bail, context, undefined);
  if (stop.kind === "resolved") {
    const value: boolean | null = stop.value;
    void value;
  }
  // @ts-expect-error: middleware requires a managed terminal
  await scope.runHook(middleware, context, 1);
  // @ts-expect-error: input must match the selected hook
  await scope.runHook(middleware, context, "wrong", { terminal });
  // @ts-expect-error: series without a terminal profile cannot accept a terminal
  await scope.runHook(series, context, 1, { terminal });
  // @ts-expect-error: middleware result is string
  const wrong: number = await scope.runHook(middleware, context, 1, { terminal });
  const terminalBail = defineHook("bail-terminal", {
    version: 1,
    strategy: "bail",
    input: integer,
    result: Type.String(),
    terminal: { input: integer, result: Type.String() },
  });
  const terminalStop = await scope.runHook(terminalBail, context, 1, { terminal });
  if (terminalStop.kind === "resolved") {
    const value: string = terminalStop.value;
    void value;
  }
  const dynamic = hookFromDescriptor(middleware.descriptor);
  const dynamicResult: unknown = await scope.runHook(dynamic, context, 1, { terminal });
  await scope.runHook(dynamic, context, 1);
  // @ts-expect-error: dynamic hook metadata does not prove a string result
  const assumed: string = await scope.runHook(dynamic, context, 1, { terminal });
  void dynamicResult;
  void assumed;
  return { result, unit, wrong };
}
