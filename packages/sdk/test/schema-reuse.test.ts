import { test } from "bun:test";
import assert from "node:assert/strict";

import {
  Type,
  assertValid,
  clonePortable,
  defineHook,
  cleanArguments,
  method,
  local,
  normalizeSchema,
  validateValue,
} from "../src/index.js";

test("contract and hook schemas retain immutable preparation without retaining values", () => {
  const source = Type.Object({ count: Type.Integer() });
  const contract = local("test.preparation", { version: 1, input: source, members: {} });
  const schema = contract.descriptor.input;
  assert.ok(Object.isFrozen(schema));
  assert.notEqual(normalizeSchema(schema), schema);
  const hook = defineHook("test.preparation", { version: 1, strategy: "series", input: source });
  assert.ok(Object.isFrozen(hook.descriptor.input));
  assert.notEqual(normalizeSchema(hook.descriptor.input), hook.descriptor.input);
  const value = { count: 1 };
  assertValid(schema, value);
  Reflect.set(value, "count", "invalid");
  assert.equal(validateValue(schema, value), false);
  Reflect.set(value, "count", Number.NaN);
  assert.throws(() => validateValue(schema, value), /number/u);
  assert.throws(() => normalizeSchema(schema, { maxDepth: 0 }), /schema_budget/u);
  assert.throws(() => normalizeSchema(schema, { maxNodes: 1 }), /schema_budget/u);
  assert.throws(() => normalizeSchema(schema, { maxDepth: -1 }), /schema_budget/u);
});

test("mutable and merely frozen external schemas are checked again", () => {
  const schema = { type: "integer" };
  assert.equal(validateValue(schema, 1), true);
  schema.type = "string";
  assert.equal(validateValue(schema, 1), false);
  const normalized = normalizeSchema(schema);
  assert.ok(typeof normalized !== "boolean");
  normalized.type = "integer";
  assert.equal(validateValue(normalized, 1), true);
  assert.throws(() => normalizeSchema(Object.freeze({ unsupported: true })), /unsupported/u);
  const property = { type: "integer" };
  const frozen = Object.freeze({ type: "object", properties: { count: property } });
  assert.equal(validateValue(frozen, { count: 1 }), true);
  property.type = "string";
  assert.equal(validateValue(frozen, { count: 1 }), false);
});

test("portable copies remain detached and reject invalid nested data without evaluating accessors", () => {
  const shared = { value: 1 };
  const source = { first: shared, second: shared, __proto__: null };
  const copy = clonePortable(source);
  assert.deepEqual(copy, { first: { value: 1 }, second: { value: 1 } });
  assert.ok(copy !== null && typeof copy === "object" && !Array.isArray(copy));
  assert.notEqual(copy.first, shared);
  assert.notEqual(copy.first, copy.second);
  let calls = 0;
  const accessor = Object.defineProperty({}, "value", {
    get() {
      calls += 1;
      return 1;
    },
    enumerable: true,
  });
  const sparse = [1, 2];
  Reflect.deleteProperty(sparse, "0");
  for (const invalid of [
    accessor,
    { value: undefined },
    { value: Number.NaN },
    new Date(),
    sparse,
  ]) {
    assert.throws(() => clonePortable({ nested: invalid }));
  }
  assert.equal(calls, 0);
  const cycle: Record<string, unknown> = {};
  cycle.child = cycle;
  assert.throws(() => clonePortable(cycle), /cyclic/u);
});

test("normalization retains detached defaults and input across repeated calls", () => {
  const contract = local("test.detached", {
    version: 1,
    input: Type.Object({
      entries: Type.Array(Type.Object({ count: Type.Integer() }), { default: [{ count: 1 }] }),
    }),
    members: {},
  });
  const input = {};
  const first = contract.normalizeInput(input);
  assert.ok(first !== null && typeof first === "object" && !Array.isArray(first));
  assert.ok(Array.isArray(first.entries));
  first.entries.push({ count: 2 });
  assert.deepEqual(input, {});
  assert.deepEqual(contract.normalizeInput(input), { entries: [{ count: 1 }] });
});

test("retained result unions do not require cleaning eligibility until cleaning is requested", () => {
  const union = Type.Union([
    Type.Object({ left: Type.Integer() }),
    Type.Object({ right: Type.Integer() }),
  ]);
  const contract = local("test.validation-only", {
    version: 1,
    input: Type.Object({}),
    members: { read: method(undefined, union) },
  });
  const member = contract.descriptor.variants[""]?.read;
  assert.ok(member?.kind === "method" && member.result !== undefined);
  assert.equal(validateValue(member.result, { right: 1 }), true);
  for (let attempt = 0; attempt < 2; attempt += 1) {
    assert.throws(() => cleanArguments(member.result, { right: 1 }), /ambiguous_normalization/u);
  }
});
