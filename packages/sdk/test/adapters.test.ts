import { test } from "bun:test";
import assert from "node:assert/strict";

import {
  ContractError,
  Type,
  canonicalJson,
  contractAdapter,
  fromDescriptor,
  hostNamed,
  local,
  method,
  prepareServeOptions,
  remote,
} from "../src/index.js";
import type { ContractAdapter, ImplementationOf } from "../src/index.js";

const Legacy = local("adapter.counter", {
  version: 1,
  input: Type.Object({}),
  members: { increment: method(Type.Integer(), Type.Integer()), count: Type.Integer() },
});
const Current = local("adapter.counter", {
  version: 2,
  input: Type.Object({}),
  members: { add: method(Type.Integer(), Type.Integer()), count: Type.Integer() },
});
const Third = local("adapter.counter", {
  version: 3,
  input: Type.Object({}),
  members: { add: method(Type.Integer(), Type.Integer()), count: Type.Integer() },
});
const adapt = (current: ImplementationOf<typeof Current>): ImplementationOf<typeof Legacy> => ({
  methods: { increment: current.methods.add },
  state: { count: current.state.count },
});
const reason =
  (expected: string) =>
  (error: unknown): boolean =>
    error instanceof ContractError && error.reason === expected;

test("contractAdapter captures canonical tokens without executing the callback", () => {
  let calls = 0;
  const callback = (current: ImplementationOf<typeof Current>) => {
    calls += 1;
    return adapt(current);
  };
  const helper = contractAdapter(Current, Legacy, callback);
  const input = [helper];
  const prepared = prepareServeOptions(Current, { lifetime: "host", linger: 0.5, accepts: input });
  assert.equal(calls, 0);
  assert.equal(Object.isFrozen(helper), true);
  assert.equal(Object.isFrozen(callback), false);
  assert.deepEqual(Reflect.ownKeys(helper), ["provider", "consumer", "adapt"]);
  assert.equal(prepared.flags, '{"lifetime":"host","linger":0.5}');
  assert.equal(prepared.adapters[0]?.provider, canonicalJson(Current.descriptor));
  assert.equal(prepared.adapters[0]?.consumer, canonicalJson(Legacy.descriptor));
  assert.equal(prepared.adapters[0]?.adapt, callback);
  assert.equal(Object.isFrozen(prepared), true);
  assert.equal(Object.isFrozen(prepared.adapters), true);
  assert.equal(Object.isFrozen(prepared.adapters[0]), true);
  input.pop();
  assert.equal(prepared.adapters.length, 1);
  assert.equal(calls, 0);
});

test("empty accepts preserves portable flags and equivalent provider tokens are accepted", () => {
  assert.deepEqual(prepareServeOptions(Current), { flags: "{}", adapters: [] });
  assert.deepEqual(prepareServeOptions(Current, { accepts: [] }), { flags: "{}", adapters: [] });
  const equivalent = {
    ...Current,
    descriptor: fromDescriptor(JSON.parse(canonicalJson(Current.descriptor))).descriptor,
  };
  const prepared = prepareServeOptions(equivalent, {
    accepts: [contractAdapter(Current, Legacy, adapt)],
  });
  assert.equal(prepared.adapters.length, 1);
  assert.equal(prepared.adapters[0]?.provider, canonicalJson(Current.descriptor));
});

test("duplicate versions and wrong providers refuse without effects or mutation", () => {
  let effects = 0;
  const callback = (current: ImplementationOf<typeof Current>) => {
    effects += 1;
    return adapt(current);
  };
  const helper = contractAdapter(Current, Legacy, callback);
  const sameVersionSubset = local("adapter.counter", {
    version: 1,
    input: Type.Object({}),
    members: { increment: method(Type.Integer(), Type.Integer()) },
  });
  const subset = contractAdapter(Current, sameVersionSubset, (current) => ({
    methods: { increment: current.methods.add },
    state: {},
  }));
  const source = { lifetime: "ref" as const, accepts: [helper, subset] };
  assert.throws(() => prepareServeOptions(Current, source), reason("accepts"));
  assert.equal(source.accepts.length, 2);
  assert.equal(source.accepts[0], helper);
  assert.throws(
    () =>
      prepareServeOptions(Third, {
        accepts: [helper],
      }),
    reason("accepts_provider"),
  );
  assert.equal(effects, 0);
  assert.equal(prepareServeOptions(Current, { accepts: [helper] }).adapters.length, 1);
});

test("adapter declarations reject moved names, kind, placement and same-version bypass", () => {
  const other = local("adapter.other", { version: 1, input: Type.Object({}), members: {} });
  assert.throws(
    () => contractAdapter(Current, other, () => ({ methods: {}, state: {} })),
    reason("placement"),
  );
  const remoteOld = remote("adapter.counter", {
    version: 1,
    input: Type.Object({}),
    host: hostNamed("old"),
    variants: { main: { methods: { read: method() } } },
  });
  const remoteNew = remote("adapter.counter", {
    version: 2,
    input: Type.Object({}),
    host: hostNamed("new"),
    variants: { main: { methods: { read: method() } } },
  });
  assert.throws(
    () => contractAdapter(remoteNew, remoteOld, (current) => current),
    reason("placement"),
  );
  assert.throws(
    () =>
      contractAdapter(Current, remoteOld, () => ({
        variant: "main" as const,
        methods: { read: () => undefined },
        state: {},
      })),
    reason("placement"),
  );
  assert.throws(() => contractAdapter(Current, Current, (current) => current), reason("accepts"));
});

test("descriptor integrity is checked without requiring a shared helper instance brand", () => {
  let calls = 0;
  const callback = (current: ImplementationOf<typeof Current>) => {
    calls += 1;
    return adapt(current);
  };
  const forged = {
    ...Current,
    descriptor: { ...Current.descriptor, fingerprint: "0".repeat(64) },
  };
  assert.throws(() => contractAdapter(forged, Legacy, callback), reason("fingerprint"));
  const structural = {
    provider: canonicalJson(Current.descriptor),
    consumer: canonicalJson(Legacy.descriptor),
    adapt: callback,
  };
  // @ts-expect-error: runtime input intentionally supplies a structural backend record instead of the public nominal adapter type.
  const accepted = prepareServeOptions(Current, { accepts: [structural] });
  assert.equal(accepted.adapters[0]?.adapt, callback);
  const corrupt = {
    ...structural,
    consumer: canonicalJson({
      ...Legacy.descriptor,
      fingerprint: "0".repeat(64),
    }),
  };
  assert.throws(() => {
    Reflect.apply(prepareServeOptions, undefined, [Current, { accepts: [corrupt] }]);
  }, reason("fingerprint"));
  assert.equal(calls, 0);
});

test("option and adapter accessors, sparse arrays and unknown flags refuse without getters", () => {
  let getters = 0;
  const getter = {
    get accepts() {
      getters += 1;
      return [];
    },
  };
  assert.throws(() => prepareServeOptions(Current, getter), reason("serve_options"));
  const entry = {
    provider: canonicalJson(Current.descriptor),
    consumer: canonicalJson(Legacy.descriptor),
    get adapt() {
      getters += 1;
      return adapt;
    },
  };
  assert.throws(() => {
    Reflect.apply(prepareServeOptions, undefined, [Current, { accepts: [entry] }]);
  }, reason("accepts"));
  const sparse: ContractAdapter<typeof Current>[] = [];
  sparse.length = 1;
  assert.throws(() => prepareServeOptions(Current, { accepts: sparse }), reason("accepts"));
  for (const options of [
    null,
    { accepts: null },
    { unknown: 1 },
    { linger: Infinity },
    { linger: -1 },
    { linger: Number.MAX_SAFE_INTEGER + 1 },
    { lifetime: "unknown" },
    { lifetime: undefined },
  ]) {
    assert.throws(
      () => {
        Reflect.apply(prepareServeOptions, undefined, [Current, options]);
      },
      (failure: unknown) => failure instanceof ContractError,
    );
  }
  assert.equal(getters, 0);
  assert.deepEqual(prepareServeOptions(Current), { flags: "{}", adapters: [] });
});

test("callbacks remain executable data and are never silently serialized as portable flags", () => {
  const callback = adapt;
  const helper = contractAdapter(Current, Legacy, callback);
  const prepared = prepareServeOptions(Current, { accepts: [helper] });
  assert.deepEqual(JSON.parse(prepared.flags), {});
  assert.equal(prepared.adapters[0]?.adapt, callback);
  assert.throws(() => canonicalJson(prepared.adapters), reason("portable"));
  assert.throws(() => {
    Reflect.apply(contractAdapter, undefined, [Current, Legacy, null]);
  }, reason("accepts"));
});
