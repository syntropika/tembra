import { test } from "bun:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";

import {
  Type,
  canonicalJson,
  defineHook,
  hookFromDescriptor,
  hookHandler,
  native,
} from "../src/index.js";
import type { AnyHookDefinition } from "../src/index.js";

// SAFETY: This checked-in versioned fixture has a shared Rust/SDK schema; cases below validate each descriptor and value.
// oxlint-disable typescript/no-unsafe-type-assertion -- Read this one authored fixture initializer using its documented shared schema.
const corpus = JSON.parse(
  await readFile(new URL("../../../fixtures/hooks/v1.json", import.meta.url), "utf-8"),
) as {
  valid: {
    id: string;
    // oxlint-enable typescript/no-unsafe-type-assertion
    descriptor: unknown;
    canonical: string;
    registry: string;
    member: unknown;
  }[];
};

test("independent TypeBox and native hook declarations match all Rust/shared golden profiles", () => {
  const input = native<{ input: true }>("example.input@1");
  const output = native<{ output: true }>("example.result@1");
  const authored: AnyHookDefinition[] = [
    defineHook("example.notify", { version: 1, strategy: "series" }),
    defineHook("example.choose", {
      version: 1,
      strategy: "bail",
      result: Type.Union([Type.Boolean(), Type.Null()]),
    }),
    defineHook("example.transform", { version: 1, strategy: "waterfall", input: Type.Integer() }),
    defineHook("example.operation", {
      version: 1,
      strategy: "middleware",
      input,
      result: output,
      terminal: { input, result: output },
    }),
    defineHook("example.parallel", {
      version: 1,
      strategy: "parallel",
      input: Type.String(),
      result: Type.Integer(),
      terminal: { input: Type.Array(Type.Integer()), result: undefined },
    }),
    defineHook("example.native-parallel", {
      version: 1,
      strategy: "parallel",
      input,
      result: native<{ item: true }>("example.item@1"),
      terminal: {
        input: native<readonly { item: true }[]>("example.items@1"),
        result: Type.Integer(),
      },
    }),
  ];
  for (const [index, value] of authored.entries()) {
    const expected = corpus.valid[index];
    assert.ok(expected !== undefined);
    assert.deepEqual(value.descriptor, expected.descriptor, expected.id);
    assert.equal(value.canonical, expected.canonical, expected.id);
    assert.equal(value.registry.name, expected.registry, expected.id);
    assert.deepEqual(value.method, expected.member, expected.id);
    assert.deepEqual(hookFromDescriptor(expected.descriptor), value, expected.id);
  }
});

test("peer metadata rejects missing fields, corrupt digests and incompatible terminal/strategy profiles", () => {
  const valid = corpus.valid.at(0)?.descriptor;
  assert.ok(valid !== null && typeof valid === "object");
  assert.throws(() => hookFromDescriptor({ ...valid, extra: true }));
  for (const key of Object.keys(valid)) {
    const value = { ...valid };
    Reflect.deleteProperty(value, key);
    assert.throws(() => hookFromDescriptor(value), key);
  }
  assert.throws(() => hookFromDescriptor({ ...valid, fingerprint: "0".repeat(64) }), {
    reason: "hook_definition",
  });
  assert.throws(
    () =>
      Reflect.apply(defineHook, undefined, [
        "bad",
        {
          version: 1,
          strategy: "middleware",
          input: Type.Integer(),
          result: Type.Integer(),
          terminal: { input: Type.String(), result: Type.Integer() },
        },
      ]),
    /terminal input/u,
  );
  assert.throws(
    () =>
      Reflect.apply(defineHook, undefined, [
        "bad",
        {
          version: 1,
          strategy: "parallel",
          result: Type.Integer(),
          terminal: { input: Type.Array(Type.String()), result: undefined },
        },
      ]),
    /vector/u,
  );
  const nullValue = defineHook("example.notify", {
    version: 1,
    strategy: "series",
    input: Type.Null(),
  });
  assert.notEqual(nullValue.descriptor.fingerprint, Reflect.get(valid, "fingerprint"));
  assert.equal(Object.isFrozen(nullValue.descriptor.input), true);
});

test("hookHandler projects raw call/recover without predecoding or acquiring authority", async () => {
  const value = Type.Integer();
  const definition = defineHook("projection", {
    version: 1,
    strategy: "middleware",
    input: value,
    result: value,
    terminal: { input: value, result: value },
  });
  const calls: unknown[] = [];
  const marker = Symbol("raw-invalid-input-must-reach-consumption");
  const handler = hookHandler(definition, async (context, args) => {
    assert.equal(context, marker);
    assert.equal(Object.isFrozen(args.next), true);
    const result: unknown = await Reflect.apply(args.next, undefined, [marker]);
    assert.equal(typeof result, "number");
    if (typeof result !== "number") {
      throw new TypeError("Expected the raw continuation result");
    }
    return await args.next.recover(result);
  });
  assert.equal(calls.length, 0);
  const raw = {
    input: 1,
    next: {
      call: async (input: unknown) => {
        calls.push(input);
        return 4;
      },
      recover: async (fallback: unknown) => {
        calls.push(fallback);
        return fallback;
      },
    },
  };
  assert.equal(await Reflect.apply(handler, undefined, [marker, raw]), 4);
  assert.deepEqual(calls, [marker, 4]);
});

test("bail false and null remain tagged stop values and metadata is deeply immutable", () => {
  const definition = defineHook("bail", {
    version: 1,
    strategy: "bail",
    result: Type.Union([Type.Boolean(), Type.Null()]),
  });
  for (const value of [false, null]) {
    const handler = hookHandler(definition, () => ({ kind: "stop", value }));
    assert.deepEqual(Reflect.apply(handler, undefined, [undefined, { input: undefined }]), {
      kind: "stop",
      value,
    });
  }
  assert.equal(Object.isFrozen(definition), true);
  assert.equal(Object.isFrozen(definition.method.args), true);
  assert.equal(
    canonicalJson(definition.descriptor),
    canonicalJson(hookFromDescriptor(definition.descriptor).descriptor),
  );
});

test("friendly Next does not inherit an ambient callable then during Promise projection", async () => {
  const value = Type.Integer();
  const definition = defineHook("ambient.then", {
    version: 1,
    strategy: "middleware",
    input: value,
    result: value,
    terminal: { input: value, result: value },
  });
  let issued: unknown;
  const handler = hookHandler(definition, (_context, args) => {
    issued = args.next;
    return 1;
  });
  Reflect.apply(handler, undefined, [
    undefined,
    {
      input: 1,
      next: {
        call: async () => 2,
        recover: async (v: unknown) => v,
      },
    },
  ]);
  const original = Object.getOwnPropertyDescriptor(Function.prototype, "then");
  let calls = 0;
  let projected: Promise<unknown> | undefined;
  try {
    // oxlint-disable-next-line no-extend-native, unicorn/no-thenable -- Regression temporarily installs ambient then and restores it before observing the projection.
    Object.defineProperty(Function.prototype, "then", {
      configurable: true,
      value(resolve: (value: unknown) => void) {
        calls += 1;
        resolve("assimilated");
      },
    });
    projected = Promise.resolve(issued);
  } finally {
    if (original === undefined) {
      Reflect.deleteProperty(Function.prototype, "then");
    } else {
      // oxlint-disable-next-line no-extend-native, unicorn/no-thenable -- Restore the exact pre-existing ambient descriptor after the regression probe.
      Object.defineProperty(Function.prototype, "then", original);
    }
  }
  assert.ok(projected !== undefined);
  assert.equal(await projected, issued);
  assert.equal(calls, 0);
});

test("hookHandler preserves capacity phase so applications can distinguish retry and recovery", async () => {
  const value = Type.Integer();
  const definition = defineHook("capacity.phase", {
    version: 1,
    strategy: "middleware",
    input: value,
    result: value,
    terminal: { input: value, result: value },
  });
  for (const phase of ["ingress", "admitted"] as const) {
    const details = { callablePhase: phase };
    const failure = Object.assign(new Error("callable capacity exhausted"), {
      code: "unavailable",
      reason: phase === "ingress" ? "jobs_limit" : "physical_jobs",
      detailsJson: JSON.stringify(details),
    });
    let calls = 0;
    let recoveries = 0;
    const handler = hookHandler(definition, async (_context, args) => {
      try {
        return await args.next(args.input);
      } catch (error) {
        assert.equal(error, failure, "projection must not erase the original structured failure");
        const received: unknown = JSON.parse(failure.detailsJson);
        assert.ok(received !== null && typeof received === "object" && "callablePhase" in received);
        if (received.callablePhase === "ingress") {
          return await args.next(args.input);
        }
        return await args.next.recover(77);
      }
    });
    // These issued ports verify the binding projection; reservation and capacity remain
    // responsibilities of the Rust core rather than this test adapter.
    const raw = {
      input: 2,
      next: {
        call: async () => {
          calls += 1;
          if (calls === 1) {
            throw failure;
          }
          return 6;
        },
        recover: async (fallback: number) => {
          recoveries += 1;
          return fallback;
        },
      },
    };
    assert.equal(
      // oxlint-disable-next-line no-await-in-loop -- Settle each controlled phase before checking its independent retry/recovery counters.
      await Reflect.apply(handler, undefined, [undefined, raw]),
      phase === "ingress" ? 6 : 77,
    );
    assert.equal(calls, phase === "ingress" ? 2 : 1);
    assert.equal(recoveries, phase === "ingress" ? 0 : 1);
    assert.deepEqual(JSON.parse(failure.detailsJson), details);
  }
});

test("unit terminal views use undefined while canonical hook metadata retains null and its digest", () => {
  const hook = defineHook("unit.terminal", {
    version: 1,
    strategy: "series",
    terminal: { input: undefined, result: undefined },
  });
  assert.deepEqual(hook.terminal, { input: undefined, result: undefined });
  assert.deepEqual(hook.descriptor.terminal, { input: null, result: null });
  assert.equal(
    hook.canonical,
    '{"format":1,"input":null,"name":"unit.terminal","result":null,"strategy":"series","terminal":{"input":null,"result":null},"version":1}',
  );
  assert.equal(
    hook.descriptor.fingerprint,
    "92477012dd5973b67cc1b9b28587b1eeceb71e7bb5914819082d99fc4c831e82",
  );
  assert.deepEqual(hookFromDescriptor(hook.descriptor).terminal, hook.terminal);
});
