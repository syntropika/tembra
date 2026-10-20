import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import test from "node:test";
import { setTimeout as delay } from "node:timers/promises";

import { Type, local, method, defineFacet, defineRegistry } from "@tembra/sdk";
import type { Cell, ManagedContext, FacetScope, EventSource, EsmArtifactV1 } from "@tembra/sdk";
import { createHost } from "@tembra/sdk/node";
import { deferredVoid } from "@tembra/testing";

const Counter = local("test.host.counter", {
  version: 1,
  input: Type.Object({}),
  members: {
    count: Type.Integer(),
    increment: method(undefined, Type.Integer()),
    wait: method(undefined, Type.Integer()),
  },
});
const Commands = defineRegistry<{ title: string }>("test.commands");
function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object";
}
function failure(error: unknown): Record<string, unknown> {
  assert.ok(isRecord(error));
  return error;
}
function dynamic(source: string): EsmArtifactV1 {
  return {
    format: "tembra-esm-v1",
    identity: "test",
    source,
    sha256: createHash("sha256").update(source).digest("hex"),
    exports: ["default"],
  };
}
function compiledCounter(step = 1) {
  return dynamic(`const Counter={descriptor:${JSON.stringify(Counter.descriptor)}};
  export default {name:'compiled-counter',uses:[],setup(_s,d){d.serve(Counter,(_i,self)=>{
    const count=self.cell('count',{schema:{type:'integer'},initial:0});
    return {state:{count},methods:{increment(ctx){return count.replace(ctx,count.value+${step}).value},wait(){return 0}}};
  });}};`);
}

const interopToken = (name: string) =>
  local(name, {
    version: 1,
    input: Type.Object({}),
    members: { run: method(Type.Integer(), Type.Integer()) },
  });

await test(
  "native callbacks, core state, roots, registries and resource cleanup",
  { timeout: 10_000 },
  async () => {
    const opened: { amount: number }[] = [];
    const closed: { amount: number }[] = [];
    const snapshots: number[] = [];
    let cell!: Cell<number>;
    let detached = false;
    let emit!: (value: number) => void;
    let savedContext!: ManagedContext;
    const source = {
      subscribe(listener: (value: number) => void) {
        emit = listener;
        return () => {
          detached = true;
        };
      },
    };
    const host = await createHost({ name: "native" });
    try {
      await host.install(
        defineFacet({
          name: "native-counter",
          uses: [],
          setup(_scope, declaration) {
            declaration.serve(
              Counter,
              (_input, self) => {
                cell = self.cell("count", { schema: Type.Integer(), initial: 0 });
                const resource = self.resource("resource", {
                  open() {
                    const value = { amount: 1 };
                    opened.push(value);
                    return value;
                  },
                  close(value) {
                    closed.push(value);
                  },
                });
                self.registerMethod(Commands, "increment", { title: "Increment" }, "increment");
                self.onEvent("events", source, (ctx, value) => {
                  cell.replace(ctx, value);
                });
                return {
                  state: { count: cell },
                  methods: {
                    increment(ctx) {
                      savedContext = ctx;
                      return cell.replace(ctx, cell.value + resource.value.amount).value;
                    },
                    async wait(ctx) {
                      await ctx.signal.cancelled;
                      ctx.check();
                      return 0;
                    },
                  },
                };
              },
              { lifetime: "host" },
            );
          },
        }),
      );
      const reference = host.hold(Counter, {});
      assert.ok(["loading", "live"].includes(reference.status));
      await reference.ready;
      const remove = reference.count.subscribe((value) => {
        snapshots.push(value);
      });
      assert.equal(await reference.increment(), 1);
      assert.throws(() => {
        savedContext.check();
      });
      const table = host.registry(Commands).table();
      assert.equal(table.entries.length, 1);
      await host.root(async (ctx) => {
        const contribution = table.get("increment");
        assert.ok(contribution?.operation);
        const result = await contribution.operation.call(ctx, { present: false });
        assert.deepEqual(result, { present: true, value: 2 });
        assert.throws(() => cell.replace({ runtime: "tembra", handle: {} }, 90));
      });
      emit(7);
      for (let i = 0; reference.count.value !== 7 && i < 50; i += 1) {
        // oxlint-disable-next-line no-await-in-loop -- Wait for the preceding event to traverse the native scheduler.
        await delay(2);
      }
      assert.equal(reference.count.value, 7);
      assert.ok(snapshots.includes(1));
      remove();
      reference.release();
      reference.release();
      assert.equal(reference.status, "released");
      await assert.rejects(reference.increment(), (error) => failure(error).code === "released");
      await host.close();
      assert.equal(detached, true);
      assert.equal(closed.length, 1);
      assert.equal(closed[0], opened[0]);
      await host.close();
    } finally {
      await host.close();
    }
  },
);

await test(
  "compiled facets use the Node realm and reload preserves state and retained references",
  { timeout: 10_000 },
  async () => {
    const host = await createHost();
    try {
      await host.load(compiledCounter());
      const reference = await host.get(Counter, {});
      assert.equal(await reference.increment(), 1);
      const changed = await host.reload([compiledCounter(2)]);
      assert.equal(changed.codeVersion, 2);
      assert.equal(await reference.increment(), 3);
      assert.equal(reference.count.value, 3);
      const corrupt = { ...compiledCounter(), sha256: "0".repeat(64) };
      await assert.rejects(host.load(corrupt));
      reference.release();
    } finally {
      await host.close();
    }
  },
);

await test(
  "installed Node → loaded Node → installed Node → loaded Node uses one Host and managed contexts",
  { timeout: 10_000 },
  async () => {
    const [A, B, C, D] = [
      interopToken("test.interop.a"),
      interopToken("test.interop.b"),
      interopToken("test.interop.c"),
      interopToken("test.interop.d"),
    ] as const;
    const host = await createHost();
    try {
      await host.load(
        dynamic(`const B={descriptor:${JSON.stringify(B.descriptor)}},C={descriptor:${JSON.stringify(C.descriptor)}},D={descriptor:${JSON.stringify(D.descriptor)}};
    export default {name:'loaded-legs',uses:[C],setup(s,d){
      d.serve(B,(_i,self)=>{const c=self.get(C,{});return {state:{},methods:{run:async(ctx,value)=>await c.run(value+1,ctx)}}});
      d.serve(D,()=>({state:{},methods:{run(ctx,value){ctx.check();return value+1}}}));
    }};`),
      );
      await host.install(
        defineFacet({
          name: "node-legs",
          uses: [B, D],
          setup(_s, d) {
            for (const [provided, dependency] of [
              [A, B],
              [C, D],
            ] as const) {
              d.serve(provided, (_i, self) => {
                const next = self.get(dependency, {});
                return {
                  state: {},
                  methods: {
                    async run(ctx, value) {
                      return await next.run(value + 1, ctx);
                    },
                  },
                };
              });
            }
          },
        }),
      );
      const reference = await host.get(A, {});
      assert.equal(await reference.run(0), 4);
      reference.release();
    } finally {
      await host.close();
    }
  },
);

await test(
  "root cancellation and shutdown retain physical callbacks through settlement",
  { timeout: 10_000 },
  async () => {
    const host = await createHost();
    let context!: ManagedContext;
    const { promise: started, resolve: entered } = deferredVoid();
    const { promise: resumed, resolve: resume } = deferredVoid();
    const pending = host.root(async (ctx) => {
      context = ctx;
      entered();
      await resumed;
      ctx.check();
    });
    const rejection = assert.rejects(pending, (error) =>
      ["aborted", "superseded"].includes(String(failure(error).code)),
    );
    await started;
    const foreign = await createHost();
    try {
      assert.throws(() => {
        foreign.cancel(context);
      });
    } finally {
      await foreign.close();
    }
    host.cancel(context);
    assert.equal(context.signal.aborted, true);
    let closed = false;
    const closing = host.close().then(() => {
      closed = true;
    });
    await delay(10);
    assert.equal(closed, false);
    resume();
    await rejection;
    await closing;
  },
);

await test(
  "independent Hosts share scheduling without sharing services or state",
  { timeout: 10_000 },
  async () => {
    const one = await createHost();
    const two = await createHost();
    try {
      await one.load(compiledCounter());
      await two.load(compiledCounter(2));
      const a = await one.get(Counter, {});
      const b = await two.get(Counter, {});
      assert.equal(await a.increment(), 1);
      assert.equal(await b.increment(), 2);
      a.release();
      b.release();
    } finally {
      await one.close();
      await two.close();
    }
  },
);

await test(
  "shutdown cancels cooperative roots and ignores valid nonportable closer results",
  { timeout: 10_000 },
  async () => {
    const host = await createHost();
    const { promise: started, resolve: entered } = deferredVoid();
    await host.install(
      defineFacet({
        name: "cleanup",
        uses: [],
        setup(_scope, d) {
          d.serve(
            Counter,
            (_i, self) => {
              self.resource("cleanup", { open: () => ({}), close: () => 1n });
              const count = self.cell("count", { schema: Type.Integer(), initial: 0 });
              return { state: { count }, methods: { increment: () => 0, wait: () => 0 } };
            },
            { lifetime: "host" },
          );
        },
      }),
    );
    const pending = host.root(async (ctx) => {
      entered();
      await ctx.signal.cancelled;
    });
    await started;
    await host.close();
    await pending;
  },
);

await test(
  "failed cleanup attempts every closer and preserves structured aggregate errors",
  { timeout: 10_000 },
  async () => {
    const host = await createHost();
    const closed: string[] = [];
    await host.install(
      defineFacet({
        name: "failing-cleanup",
        uses: [],
        setup(_scope, d) {
          d.serve(
            Counter,
            (_i, self) => {
              for (const name of ["first", "second"]) {
                self.resource(name, {
                  open: () => ({}),
                  close() {
                    closed.push(name);
                    throw new Error(`${name} failed`);
                  },
                });
              }
              const count = self.cell("count", { schema: Type.Integer(), initial: 0 });
              return { state: { count }, methods: { increment: () => 0, wait: () => 0 } };
            },
            { lifetime: "host" },
          );
        },
      }),
    );
    await assert.rejects(host.close(), (error) => {
      const value = failure(error);
      const details = failure(value.details);
      const { errors } = details;
      return (
        value.code === "unavailable" &&
        value.reason === "cleanup" &&
        Array.isArray(errors) &&
        errors.length > 0 &&
        JSON.stringify(details).includes("first failed") &&
        JSON.stringify(details).includes("second failed")
      );
    });
    assert.deepEqual(closed, ["second", "first"]);
  },
);

await test(
  "method callbacks preserve application realm and structured errors",
  { timeout: 10_000 },
  async () => {
    const host = await createHost();
    const processIdentity = process;
    const Contract = local("test.native.realm", {
      version: 1,
      input: Type.Object({}),
      members: { run: method(undefined, Type.Integer()) },
    });
    try {
      await host.install(
        defineFacet({
          name: "realm",
          uses: [],
          setup(_s, d) {
            d.serve(Contract, () => ({
              state: {},
              methods: {
                run() {
                  assert.equal(process, processIdentity);
                  throw Object.assign(new Error("provider failed"), {
                    code: "forbidden",
                    reason: "policy",
                    detailsJson: '{"allowed":false}',
                  });
                },
              },
            }));
          },
        }),
      );
      const reference = await host.get(Contract, {});
      await assert.rejects(
        reference.run(),
        (error) =>
          failure(error).code === "forbidden" &&
          failure(error).reason === "policy" &&
          failure(failure(error).details).allowed === false,
      );
      reference.release();
    } finally {
      await host.close();
    }
  },
);

await test(
  "close before the first scheduler poll rejects queued roots without hanging",
  { timeout: 10_000 },
  async () => {
    const host = await createHost();
    const pending = host.root(async (ctx) => {
      await ctx.signal.cancelled;
    });
    const failed = assert.rejects(pending, (error) => failure(error).reason === "host_closed");
    await host.close();
    await failed;
  },
);

await test(
  "arbitrary thrown values always settle their native callback",
  { timeout: 10_000 },
  async () => {
    const host = await createHost();
    try {
      await assert.rejects(
        host.root(async () => {
          // oxlint-disable-next-line typescript/only-throw-error -- Validate arbitrary thrown JavaScript values, including hostile getters.
          throw new Proxy(
            {},
            {
              get() {
                throw new Error("error getter failed");
              },
            },
          );
        }),
        (error) => failure(error).reason === "javascript",
      );
      assert.equal(host.diagnostics().callbackJobs, 0);
    } finally {
      await host.close();
    }
  },
);

await test(
  "refused asynchronous declarations observe their rejected Promises",
  { timeout: 10_000 },
  async () => {
    const host = await createHost();
    try {
      await assert.rejects(
        host.install(
          defineFacet({
            name: "async-setup",
            uses: [],
            // @ts-expect-error The runtime rejects asynchronous declarations received from JavaScript.
            async setup() {
              throw new Error("setup failed");
            },
          }),
        ),
        (error) => failure(error).reason === "async_declaration",
      );
      await delay(5);
    } finally {
      await host.close();
    }
  },
);

await test(
  "released event subscriptions reclaim native sink and wrapper handles",
  { timeout: 10_000, skip: typeof globalThis.gc !== "function" },
  async () => {
    const host = await createHost();
    const Contract = local("test.native.events", {
      version: 1,
      input: Type.Object({ key: Type.Integer() }),
      members: {},
    });
    const source = {
      subscribe() {
        return () => undefined;
      },
    };
    try {
      await host.install(
        defineFacet({
          name: "events",
          uses: [],
          setup(_s, d) {
            d.serve(Contract, (_i, self) => {
              self.onEvent("events", source, () => undefined);
              return { state: {}, methods: {} };
            });
          },
        }),
      );
      for (let key = 0; key < 10; key += 1) {
        // oxlint-disable-next-line no-await-in-loop -- Distinct objects must activate and dispose in sequence.
        const reference = await host.get(Contract, { key });
        reference.release();
      }
      for (let attempt = 0; attempt < 10 && host.diagnostics().handles > 0; attempt += 1) {
        globalThis.gc?.();
        // oxlint-disable-next-line no-await-in-loop -- Allow finalization and preceding core disposal to finish.
        await delay(10);
      }
      assert.equal(host.diagnostics().handlesByKind.sink ?? 0, 0);
      // An optimizing engine may retain the last local reference until this test returns.
      assert.ok(host.diagnostics().handles <= 1);
    } finally {
      await host.close();
    }
  },
);

await test(
  "async state initialization, derivation, managed listeners and resource events use core scopes",
  { timeout: 10_000 },
  async () => {
    const host = await createHost();
    const Contract = local("test.native.activities", {
      version: 1,
      input: Type.Object({}),
      members: { value: Type.Integer(), run: method(undefined, Type.Integer()) },
    });
    const seen: number[] = [];
    let emit!: (value: number) => void;
    let taskStopped = false;
    let detached = false;
    const source = {
      subscribe(listener: (value: number) => void) {
        emit = listener;
        return () => {
          detached = true;
        };
      },
    };
    try {
      await host.install(
        defineFacet({
          name: "activities",
          uses: [],
          setup(_s, d) {
            d.serve(Contract, (_i, self) => {
              const cell = self.cell("value", {
                schema: Type.Integer(),
                async initialize(ctx) {
                  ctx.check();
                  await delay(1);
                  return 2;
                },
              });
              const doubled = self.derive(
                [cell],
                Type.Integer(),
                ([snapshot]) => snapshot.value * 2,
              );
              // @ts-expect-error Runtime callback settlement ignores nonportable completion values from JavaScript.
              self.on("observe", cell, (_ctx, snapshot) => {
                seen.push(snapshot.value);
                return 1n;
              });
              const eventResource = self.resource("events", {
                open: () => source,
                close: () => undefined,
              });
              self.onResource(
                "resource-events",
                eventResource,
                (value): EventSource<number> => value,
                // @ts-expect-error Runtime callback settlement ignores nonportable completion values from JavaScript.
                (ctx: ManagedContext, value: number) => {
                  cell.replace(ctx, value);
                  return 1n;
                },
              );
              // @ts-expect-error Runtime task settlement ignores nonportable completion values from JavaScript.
              self.task(async (scope) => {
                await scope.signal.cancelled;
                taskStopped = true;
                return 1n;
              });
              return {
                state: { value: doubled },
                methods: {
                  run(ctx) {
                    cell.replace(ctx, cell.value + 1);
                    return doubled.value;
                  },
                },
              };
            });
          },
        }),
      );
      const reference = await host.get(Contract, {});
      const initialValue = reference.value.value;
      assert.equal(initialValue, 4);
      assert.equal(await reference.run(), 6);
      emit(5);
      for (let attempt = 0; reference.value.value !== 10 && attempt < 50; attempt += 1) {
        // oxlint-disable-next-line no-await-in-loop -- Await the preceding resource event's core delivery.
        await delay(2);
      }
      assert.equal(reference.value.value, 10);
      assert.ok(seen.includes(3));
      reference.release();
      await host.close();
      assert.equal(taskStopped, true);
      assert.equal(detached, true);
    } finally {
      await host.close();
    }
  },
);

await test("portable hooks and explicit JavaScript opaque capabilities", async () => {
  const { portableHooks, javascriptOpaque, hookBoundaries, hookPhysicalSettlement } =
    await import("@tembra/testing/conformance");
  const host = await createHost();
  try {
    await portableHooks(host);
    await javascriptOpaque(host);
    await hookBoundaries(host);
    await hookPhysicalSettlement(host);
  } finally {
    await host.close();
  }
});

await test("portable middleware and opaque identity cross installed and loaded Node facets", async () => {
  const { defineHook, hookHandler, registerHook, native, opaque } = await import("@tembra/sdk");
  const hook = defineHook("test.interop.middleware", {
    version: 1,
    strategy: "middleware",
    input: Type.Integer(),
    result: Type.Integer(),
    terminal: { input: Type.Integer(), result: Type.Integer() },
  });
  const Native = local("test.interop.native-hook", {
    version: 1,
    input: Type.Object({}),
    members: { around: hook.method, terminal: method(Type.Integer(), Type.Integer()) },
  });
  const Compiled = local("test.interop.compiled-hook", {
    version: 1,
    input: Type.Object({}),
    members: { around: hook.method, run: method(Type.Integer(), Type.Integer()) },
  });
  const nominal = native<{ identity: symbol }>("test.interop.opaque@1");
  const Opaque = local("test.interop.opaque", {
    version: 1,
    input: Type.Object({}),
    members: { value: opaque<{ identity: symbol }>(nominal.native) },
  });
  const RoundTrip = local("test.interop.opaque-roundtrip", {
    version: 1,
    input: Type.Object({}),
    members: { value: opaque<{ identity: symbol }>(nominal.native) },
  });
  const registry = defineRegistry("test.interop.terminals");
  const value = { identity: Symbol("node realm") };
  const host = await createHost();
  let scope!: FacetScope;
  try {
    host.registerHook(hook);
    host.registerOpaque(nominal);
    await host.install(
      defineFacet({
        name: "interop-node-hooks",
        uses: [],
        setup(s, d) {
          scope = s;
          d.serve(
            Native,
            (_i, self) => {
              registerHook(self, hook, "native", "around", { priority: 0 });
              self.registerMethod(registry, "terminal", {}, "terminal");
              return {
                state: {},
                methods: {
                  around: hookHandler(
                    hook,
                    async (_ctx, { input, next }) => (await next(input + 1)) + 10,
                  ),
                  terminal: (_ctx, input) => input * 3,
                },
              };
            },
            { lifetime: "host" },
          );
          d.serve(Opaque, (_i, self) => ({
            state: {},
            methods: {},
            opaque: { value: self.resource("value", { open: () => value }) },
          }));
        },
      }),
    );
    await host.load(
      dynamic(`
      const Hook=${JSON.stringify({ descriptor: hook.descriptor, registry: hook.registry, input: hook.input, result: hook.result, terminal: hook.terminal })};
      const Compiled={descriptor:${JSON.stringify(Compiled.descriptor)}};
      const Opaque={descriptor:${JSON.stringify(Opaque.descriptor)}};
      const RoundTrip={descriptor:${JSON.stringify(RoundTrip.descriptor)}};
      const registry=${JSON.stringify(registry)};
      export default {name:'interop-loaded-hooks',uses:[Opaque],setup(scope,d){
        d.serve(Compiled,(_i,self)=>{
          self.registerMethod(Hook.registry,'loaded',{hook:Hook.descriptor.fingerprint,priority:1,profile:1},'around');
          return {state:{},methods:{around:async(_ctx,{input,next})=>(await next.call(input*2))+20,run:async(ctx,input)=>{
            const terminal=scope.registry(registry).table(ctx).get('terminal').operation;
            return await scope.runHook(Hook,ctx,input,{terminal});
          }}};
        },{lifetime:'host'});
        d.serve(RoundTrip,(_i,self)=>{
          const dependency=self.get(Opaque,{});
          const resource=self.resource('roundtrip',{deps:[dependency],open:()=>dependency.value});
          return {state:{},methods:{},opaque:{value:resource}};
        });
      }};
    `),
    );
    const compiled = await host.get(Compiled, {});
    assert.equal(await compiled.run(1), 42);
    await host.root(async (ctx) => {
      const contribution = scope.registry(registry).table(ctx).get("terminal");
      assert.ok(contribution?.operation);
      const terminal = contribution.operation;
      assert.equal(await scope.runHook(hook, ctx, 1, { terminal }), 42);
    });
    const roundtrip = await host.get(RoundTrip, {});
    assert.equal(roundtrip.value, value);
    roundtrip.release();
    compiled.release();
  } finally {
    await host.close();
  }
});

await test("Next ingress refusal preserves retry authority and reports its phase", async () => {
  const { defineHook, hookHandler, registerHook } = await import("@tembra/sdk");
  const hook = defineHook("test.next.ingress", {
    version: 1,
    strategy: "middleware",
    input: Type.Integer(),
    result: Type.Integer(),
    terminal: { input: Type.Integer(), result: Type.Integer() },
  });
  const Service = local("test.next.ingress", {
    version: 1,
    input: Type.Object({}),
    members: { around: hook.method, terminal: method(Type.Integer(), Type.Integer()) },
  });
  const registry = defineRegistry("test.next.ingress.terminals");
  const host = await createHost();
  let scope!: FacetScope;
  try {
    host.registerHook(hook);
    await host.install(
      defineFacet({
        name: "next-ingress",
        uses: [],
        setup(s, d) {
          scope = s;
          d.serve(
            Service,
            (_i, self) => {
              registerHook(self, hook, "around", "around");
              self.registerMethod(registry, "terminal", {}, "terminal");
              return {
                state: {},
                methods: {
                  around: hookHandler(hook, async (_ctx, { input, next }) => {
                    const roots = Array.from({ length: 126 }, async () => {
                      await host.root(() => undefined);
                    });
                    await assert.rejects(
                      // oxlint-disable-next-line node/callback-return -- Next is a scoped hook continuation, not an error-first callback.
                      next(input),
                      (error) =>
                        failure(JSON.parse(String(failure(error).detailsJson))).callablePhase ===
                        "ingress",
                    );
                    await Promise.all(roots);
                    return await next(input);
                  }),
                  terminal: (_ctx, input) => input,
                },
              };
            },
            { lifetime: "host" },
          );
        },
      }),
    );
    await host.root(async (ctx) => {
      const contribution = scope.registry(registry).table(ctx).get("terminal");
      assert.ok(contribution?.operation);
      const terminal = contribution.operation;
      assert.equal(await scope.runHook(hook, ctx, 9, { terminal }), 9);
    });
  } finally {
    await host.close();
  }
});
