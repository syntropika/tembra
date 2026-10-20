/* oxlint-disable node/callback-return -- Next is a managed hook continuation, not an error-first Node callback. */
import {
  Type,
  defineFacet,
  defineHook,
  hookHandler,
  local,
  method,
  native,
  opaque,
  registerHook,
} from "@tembra/sdk";
import type { FacetScope, TembraHost, Next } from "@tembra/sdk";

import { check } from "./assertions.ts";

/** Runs the same real binding conformance sequence on Node and in a browser. */
export async function portableHooks(host: TembraHost): Promise<void> {
  const series = defineHook("test.host.series", {
    version: 1,
    strategy: "series",
    input: Type.Integer(),
  });
  const bail = defineHook("test.host.bail", {
    version: 1,
    strategy: "bail",
    input: Type.Integer(),
    result: Type.Boolean(),
  });
  const waterfall = defineHook("test.host.waterfall", {
    version: 1,
    strategy: "waterfall",
    input: Type.Integer(),
  });
  const parallel = defineHook("test.host.parallel", {
    version: 1,
    strategy: "parallel",
    input: Type.Integer(),
    result: Type.Integer(),
    terminal: { input: Type.Array(Type.Integer()), result: Type.Integer() },
  });
  const middleware = defineHook("test.host.middleware", {
    version: 1,
    strategy: "middleware",
    input: Type.Integer(),
    result: Type.Integer(),
    terminal: { input: Type.Integer(), result: Type.Integer() },
  });
  const unit = defineHook("test.host.unit", { version: 1, strategy: "series" });
  const seriesThen = defineHook("test.host.series-terminal", {
    version: 1,
    strategy: "series",
    input: Type.Integer(),
    terminal: { input: Type.Integer(), result: Type.Integer() },
  });
  const waterfallThen = defineHook("test.host.waterfall-terminal", {
    version: 1,
    strategy: "waterfall",
    input: Type.Integer(),
    terminal: { input: Type.Integer(), result: Type.Integer() },
  });
  const bailFallback = defineHook("test.host.bail-terminal", {
    version: 1,
    strategy: "bail",
    input: Type.Integer(),
    result: Type.Integer(),
    terminal: { input: Type.Integer(), result: Type.Integer() },
  });
  const unresolved = defineHook("test.host.unresolved", {
    version: 1,
    strategy: "bail",
    result: Type.Boolean(),
  });
  const nullable = defineHook("test.host.null-parallel", {
    version: 1,
    strategy: "parallel",
    input: Type.Null(),
    result: Type.Null(),
  });
  const Hooks = local("test.host.hooks", {
    version: 1,
    input: Type.Object({}),
    members: {
      series: series.method,
      bail: bail.method,
      waterfall: waterfall.method,
      parallel: parallel.method,
      middleware: middleware.method,
      unit: unit.method,
      seriesThen: seriesThen.method,
      waterfallThen: waterfallThen.method,
      bailFallback: bailFallback.method,
      nullable: nullable.method,
    },
  });
  const Terminal = local("test.host.terminal", {
    version: 1,
    input: Type.Object({}),
    members: {
      run: method(Type.Integer(), Type.Integer()),
      sum: method(Type.Array(Type.Integer()), Type.Integer()),
    },
  });
  const terminals = { name: "test.host.terminals", conflict: "error" as const };
  const order: number[] = [];
  let scope: FacetScope | undefined;
  let retained: Next<number, number> | undefined;
  for (const hook of [
    series,
    bail,
    waterfall,
    parallel,
    middleware,
    unit,
    seriesThen,
    waterfallThen,
    bailFallback,
    unresolved,
    nullable,
  ]) {
    host.registerHook(hook);
    host.registerHook(hook);
  }
  await host.install(
    defineFacet({
      name: "hook-contributions",
      uses: [],
      setup(s, d) {
        scope = s;
        d.serve(
          Hooks,
          (_input, self) => {
            for (const [name, hook] of Object.entries({
              series,
              bail,
              waterfall,
              parallel,
              middleware,
              unit,
              seriesThen,
              waterfallThen,
              bailFallback,
              nullable,
            })) {
              registerHook(self, hook, name, name);
            }
            return {
              state: {},
              methods: {
                seriesThen: hookHandler(seriesThen, () => undefined),
                waterfallThen: hookHandler(waterfallThen, (_ctx, { input }) => input + 1),
                bailFallback: hookHandler(bailFallback, () => ({ kind: "continue" })),
                nullable: hookHandler(nullable, (_ctx, { input }) => input),
                series: hookHandler(series, (_ctx, { input }) => {
                  order.push(input);
                  return undefined;
                }),
                bail: hookHandler(bail, () => ({ kind: "stop", value: false })),
                waterfall: hookHandler(waterfall, (_ctx, { input }) => input + 2),
                parallel: hookHandler(parallel, (_ctx, { input }) => input * 3),
                middleware: hookHandler(middleware, async (_ctx, { input, next }) => {
                  retained = next;
                  return (await next(input + 1)) + 2;
                }),
                unit: hookHandler(unit, () => {
                  order.push(99);
                  return undefined;
                }),
              },
            };
          },
          { lifetime: "host" },
        );
        d.serve(
          Terminal,
          (_input, self) => {
            self.registerMethod(terminals, "run", {}, "run");
            self.registerMethod(terminals, "sum", {}, "sum");
            return {
              state: {},
              methods: {
                run: (_ctx, input) => input * 2,
                sum: (_ctx, input) => input.reduce((total, value) => total + value, 0),
              },
            };
          },
          { lifetime: "host" },
        );
      },
    }),
  );
  const admitted = scope;
  check(admitted, "facet scope issued");
  await host.root(async (ctx) => {
    await admitted.runHook(series, ctx, 4);
    await admitted.runHook(unit, ctx, undefined);
    check(order.join(",") === "4,99", "series and absent payload");
    const stop = await admitted.runHook(bail, ctx, 0);
    check(stop.kind === "resolved" && !stop.value, "bail false remains resolved");
    check((await admitted.runHook(waterfall, ctx, 3)) === 5, "waterfall output");
    const table = admitted.registry(terminals).table(ctx);
    const terminal = table.get("run")?.operation;
    const sum = table.get("sum")?.operation;
    check(terminal && sum, "managed terminals issued");
    check((await admitted.runHook(seriesThen, ctx, 3, { terminal })) === 6, "series terminal");
    check(
      (await admitted.runHook(waterfallThen, ctx, 3, { terminal })) === 8,
      "waterfall terminal",
    );
    const fallback = await admitted.runHook(bailFallback, ctx, 3, { terminal });
    check(fallback.kind === "resolved" && fallback.value === 6, "bail fallback terminal");
    const missing = await admitted.runHook(unresolved, ctx, undefined);
    check(missing.kind === "unresolved", "bail unresolved");
    const nulls = await admitted.runHook(nullable, ctx, null);
    check(nulls.length === 1 && nulls[0] === null, "explicit null hook payloads");
    let preflight = false;
    try {
      await admitted.runHook(middleware, ctx, 3, { terminal: sum });
    } catch {
      preflight = true;
    }
    check(preflight, "incompatible terminal refuses before contribution entry");
    check(
      (await admitted.runHook(parallel, ctx, 3, { terminal: sum })) === 9,
      "parallel managed terminal",
    );
    check(
      (await admitted.runHook(middleware, ctx, 3, { terminal })) === 10,
      "middleware next and terminal",
    );
    check(
      (await admitted.runHook(middleware, ctx, 3, { terminal })) === 10,
      "terminal wrapper can be reused",
    );
  });
  let expired = false;
  try {
    await retained?.(1);
  } catch {
    expired = true;
  }
  check(expired, "retained next loses authority");
}

export async function javascriptOpaque(host: TembraHost): Promise<void> {
  const schema = native<{ readonly marker: symbol }>("test.javascript.value@1");
  const Token = local("test.host.opaque", {
    version: 1,
    input: Type.Object({}),
    members: { value: opaque<{ readonly marker: symbol }>(schema.native) },
  });
  let value: { readonly marker: symbol } | undefined;
  host.registerOpaque(schema);
  host.registerOpaque(schema);
  await host.install(
    defineFacet({
      name: "javascript-opaque",
      uses: [],
      setup(_s, d) {
        d.serve(Token, (_i, self) => {
          const resource = self.resource("capability", {
            open: () => {
              value = { marker: Symbol("identity") };
              return value;
            },
          });
          return { state: {}, methods: {}, opaque: { value: resource } };
        });
      },
    }),
  );
  const reference = await host.get(Token, {});
  check(reference.value === value, "opaque capability preserves JavaScript identity");
  reference.release();
  let stale = false;
  try {
    void reference.value;
  } catch {
    stale = true;
  }
  check(stale, "released reference cannot read opaque capability");
  const refusal = async (forged: boolean): Promise<void> => {
    const Unsafe = local(`test.host.opaque-refusal.${forged}`, {
      version: 1,
      input: Type.Object({}),
      members: { value: opaque(forged ? schema.native : "test.javascript.unregistered@1") },
    });
    let opened = false;
    await host.install(
      defineFacet({
        name: `opaque-refusal-${forged}`,
        uses: [],
        setup(_scope, declaration) {
          declaration.serve(Unsafe, (_input, self) => {
            const resource = self.resource<unknown>("value", {
              open: () => {
                opened = true;
                return {};
              },
            });
            return {
              state: {},
              methods: {},
              opaque: { value: forged ? { ...resource } : resource },
            };
          });
        },
      }),
    );
    let refused = false;
    try {
      const unexpected = await host.get(Unsafe, {});
      unexpected.release();
    } catch {
      refused = true;
    }
    check(
      refused && !opened,
      forged
        ? "structural resource copies are not capabilities"
        : "unregistered opaque nominals refuse before resource activation",
    );
  };
  await refusal(false);
  await refusal(true);
}

export async function hookBoundaries(host: TembraHost): Promise<void> {
  const unitBail = defineHook("test.host.unit-bail", { version: 1, strategy: "bail" });
  const unitParallel = defineHook("test.host.unit-parallel", { version: 1, strategy: "parallel" });
  const middleware = defineHook("test.host.recovery", {
    version: 1,
    strategy: "middleware",
    input: Type.Integer(),
    result: Type.Integer(),
    terminal: { input: Type.Integer(), result: Type.Integer() },
  });
  const Hooks = local("test.host.boundary-hooks", {
    version: 1,
    input: Type.Object({}),
    members: { bail: unitBail.method, parallel: unitParallel.method, around: middleware.method },
  });
  const Terminal = local("test.host.boundary-terminal", {
    version: 1,
    input: Type.Object({}),
    members: { run: method(Type.Integer(), Type.Integer()) },
  });
  const registry = { name: "test.host.boundary-terminals", conflict: "error" as const };
  let scope: FacetScope | undefined;
  host.registerHook(unitBail);
  host.registerHook(unitParallel);
  host.registerHook(middleware);
  await host.install(
    defineFacet({
      name: "boundary-hooks",
      uses: [],
      setup(s, d) {
        scope = s;
        d.serve(
          Hooks,
          (_i, self) => {
            registerHook(self, unitBail, "bail", "bail");
            registerHook(self, unitParallel, "parallel", "parallel");
            registerHook(self, middleware, "around", "around");
            return {
              state: {},
              methods: {
                bail: hookHandler(unitBail, () => ({ kind: "stop", value: undefined })),
                parallel: hookHandler(unitParallel, () => undefined),
                around: hookHandler(middleware, async (_ctx, { input, next }) => {
                  if (input === 3) {
                    const value = await next(0);
                    let refused = false;
                    try {
                      await next(0);
                    } catch {
                      refused = true;
                    }
                    check(refused, "next is single-use");
                    return value;
                  }
                  try {
                    return await next(input);
                  } catch {
                    if (input === 2) {
                      return await next.recover(42);
                    }
                    return 42;
                  }
                }),
              },
            };
          },
          { lifetime: "host" },
        );
        d.serve(
          Terminal,
          (_i, self) => {
            self.registerMethod(registry, "run", {}, "run");
            return {
              state: {},
              methods: {
                run: (_ctx, input) => {
                  if (input > 0) {
                    throw new Error("terminal failed");
                  }
                  return 7;
                },
              },
            };
          },
          { lifetime: "host" },
        );
      },
    }),
  );
  const admitted = scope;
  check(admitted, "boundary scope");
  await host.root(async (ctx) => {
    const stopped = await admitted.runHook(unitBail, ctx, undefined);
    check(stopped.kind === "resolved" && stopped.value === undefined, "absent bail result");
    const vector = await admitted.runHook(unitParallel, ctx, undefined);
    check(vector.length === 1 && vector[0] === undefined, "absent parallel results");
    const terminal = admitted.registry(registry).table(ctx).get("run")?.operation;
    check(terminal, "boundary terminal");
    let refused = false;
    try {
      await admitted.runHook(middleware, ctx, 1, { terminal });
    } catch {
      refused = true;
    }
    check(refused, "catch without recover cannot acknowledge failure");
    check((await admitted.runHook(middleware, ctx, 2, { terminal })) === 42, "explicit recovery");
    check(
      (await admitted.runHook(middleware, ctx, 3, { terminal })) === 7,
      "secondary misuse cannot overwrite primary",
    );
  });
}

export async function hookPhysicalSettlement(host: TembraHost): Promise<void> {
  const hook = defineHook("test.host.physical", {
    version: 1,
    strategy: "middleware",
    input: Type.Integer(),
    result: Type.Integer(),
    terminal: { input: Type.Integer(), result: Type.Integer() },
  });
  const Worker = local("test.host.hook-worker", {
    version: 1,
    input: Type.Object({}),
    members: { wait: method(undefined, Type.Integer()) },
  });
  const Terminal = local("test.host.physical-terminal", {
    version: 1,
    input: Type.Object({}),
    members: { run: method(Type.Integer(), Type.Integer()) },
  });
  const registry = { name: "test.host.physical-terminals", conflict: "error" as const };
  let scope: FacetScope | undefined;
  let release: (() => void) | undefined;
  let entered: (() => void) | undefined;
  // oxlint-disable-next-line promise/avoid-new -- The test owns the physical settlement gate.
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  // oxlint-disable-next-line promise/avoid-new -- The test waits for actual dependent method entry.
  const started = new Promise<void>((resolve) => {
    entered = resolve;
  });
  host.registerHook(hook);
  await host.install(
    defineFacet({
      name: "physical-hooks",
      uses: [Worker],
      setup(s, d) {
        scope = s;
        d.serve(Worker, () => ({
          state: {},
          methods: {
            wait: async () => {
              entered?.();
              await gate;
              return 1;
            },
          },
        }));
        d.serve(
          Terminal,
          (_i, self) => {
            const worker = self.get(Worker, {});
            self.registerMethod(registry, "run", {}, "run");
            return {
              state: {},
              methods: {
                run: (ctx, input) => {
                  worker.wait(ctx).catch(() => undefined);
                  return input;
                },
              },
            };
          },
          { lifetime: "host" },
        );
      },
    }),
  );
  const admitted = scope;
  check(admitted, "physical scope");
  let settled = false;
  const run = host.root(async (ctx) => {
    const terminal = admitted.registry(registry).table(ctx).get("run")?.operation;
    check(terminal, "physical terminal");
    return await admitted.runHook(hook, ctx, 8, { terminal });
  });
  const observed = run.then((value) => {
    settled = true;
    return value;
  });
  await started;
  await Promise.resolve();
  await Promise.resolve();
  check(!settled, "hook waits for terminal detached managed work");
  release?.();
  check((await observed) === 8, "hook finishes after physical terminal settlement");
}
