import { test } from "bun:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";

import { Value } from "typebox/value";

import { assertPortable, canonicalJson, normalizeSchema, validateValue } from "../src/index.js";
import type { Schema } from "../src/index.js";
import { object } from "../src/schema.js";

/** Reproduces the former TypeBox adapter for comparison over the portable profile. */
function typeboxSchema(schema: Schema): Schema {
  if (typeof schema === "boolean") {
    return schema;
  }
  const result = { ...schema };
  delete result.minLength;
  delete result.maxLength;
  delete result.uniqueItems;
  if (result.properties !== undefined) {
    result.properties = Object.fromEntries(
      Object.entries(object(result.properties)).map(([key, value]) => [
        key,
        typeboxSchema(normalizeSchema(value)),
      ]),
    );
  }
  if (result.items !== undefined) {
    result.items = typeboxSchema(normalizeSchema(result.items));
  }
  if (Array.isArray(result.anyOf)) {
    result.anyOf = result.anyOf.map((branch) => typeboxSchema(normalizeSchema(branch)));
  }
  return result;
}
function formerCheck(schema: Schema, value: unknown): boolean {
  if (!Value.Check(typeboxSchema(schema), value)) {
    return false;
  }
  if (typeof schema === "boolean") {
    return schema;
  }
  if (
    Array.isArray(value) &&
    schema.uniqueItems === true &&
    new Set(value.map(canonicalJson)).size !== value.length
  ) {
    return false;
  }
  if (typeof value === "string") {
    // oxlint-disable-next-line typescript/no-misused-spread -- the portable profile compares Unicode scalar counts with the prior adapter
    const { length } = [...value];
    if (
      (typeof schema.minLength === "number" && length < schema.minLength) ||
      (typeof schema.maxLength === "number" && length > schema.maxLength)
    ) {
      return false;
    }
  }
  if (
    Array.isArray(schema.anyOf) &&
    !schema.anyOf.some((branch) => formerCheck(normalizeSchema(branch), value))
  ) {
    return false;
  }
  if (
    Array.isArray(value) &&
    schema.items !== undefined &&
    !value.every((item) => formerCheck(normalizeSchema(schema.items), item))
  ) {
    return false;
  }
  return formerObjectCheck(schema, value);
}
function formerObjectCheck(schema: Record<string, unknown>, value: unknown): boolean {
  if (
    value !== null &&
    typeof value === "object" &&
    !Array.isArray(value) &&
    schema.properties !== undefined
  ) {
    for (const [key, property] of Object.entries(object(schema.properties))) {
      if (
        Object.hasOwn(value, key) &&
        !formerCheck(normalizeSchema(property), object(value)[key])
      ) {
        return false;
      }
    }
  }
  return true;
}

test("portable checker preserves the adapted TypeBox result across supported keyword combinations", () => {
  const schemas: unknown[] = [
    true,
    false,
    {},
    { const: null },
    { const: { b: [1, false], a: "x" } },
    { enum: [null, true, 2, { a: 1 }, [1, 2]] },
    ...["null", "boolean", "number", "integer", "string", "array", "object"].map((type) => ({
      type,
    })),
    ...[-1, 0, 1].flatMap((bound) =>
      ["minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum"].map((keyword) => ({
        [keyword]: bound,
      })),
    ),
    { type: "number", minimum: -1, exclusiveMaximum: 2, enum: [-1, 0, 1, 2] },
    { type: ["integer", "null"], minimum: 0 },
    {
      anyOf: [
        { type: "integer", minimum: 0 },
        { type: "string", minLength: 1 },
      ],
    },
    { type: "string", minLength: 1, maxLength: 2 },
    { type: "array", minItems: 1, maxItems: 2, uniqueItems: true },
    { type: "array", items: { anyOf: [{ type: "null" }, { type: "number", minimum: 0 }] } },
    { type: "array", items: false },
    { type: "object", minProperties: 1, maxProperties: 2 },
    {
      type: "object",
      properties: { a: { type: "integer" } },
      required: ["a"],
      additionalProperties: false,
    },
    { properties: { a: false }, additionalProperties: true },
    { additionalProperties: false },
    {
      type: "object",
      properties: { a: { type: "array", items: { type: "string", maxLength: 1 } } },
    },
  ];
  const values: unknown[] = [
    null,
    true,
    false,
    -2,
    -1,
    -0.5,
    -0,
    0,
    0.5,
    1,
    2,
    "",
    "a",
    "ab",
    "abc",
    "\uD83D\uDE80",
    "a\u0301",
    [],
    [1],
    [1, 2],
    [1, 1],
    [null, 0],
    [-1],
    ["a", "b"],
    ["a", "ab"],
    [{ a: 1 }, { a: 1 }],
    {},
    { a: 1 },
    { a: 1.5 },
    { a: "x" },
    { a: null },
    { b: 1 },
    { a: 1, b: 2 },
    { a: 1, b: 2, c: 3 },
    { a: ["a"] },
    { a: ["ab"] },
    { b: [1, false], a: "x" },
    { a: "x", b: [1, false] },
  ];
  let comparisons = 0;
  for (const schema of schemas) {
    for (const value of values) {
      const normalized = normalizeSchema(schema);
      assert.equal(
        validateValue(schema, value),
        formerCheck(normalized, value),
        `${JSON.stringify(schema)} against ${JSON.stringify(value)}`,
      );
      comparisons += 1;
    }
  }
  assert.ok(comparisons > 1000);
});

test("portable checker preserves every shared validation corpus result", async () => {
  const corpus: unknown = JSON.parse(
    await readFile(
      new URL("../../../testing/fixtures/contracts/v1.json", import.meta.url),
      "utf-8",
    ),
  );
  const validation: unknown = object(corpus).validation;
  assert.ok(Array.isArray(validation));
  const vectors: unknown[] = validation;
  for (const entry of vectors) {
    const vector = object(entry);
    assert.equal(typeof vector.name, "string");
    const name = String(vector.name);
    let actual = false;
    let former = false;
    try {
      actual = validateValue(vector.schema, vector.value);
    } catch {
      /* Invalid portable inputs reject. */
    }
    try {
      assertPortable(vector.value);
      former = formerCheck(normalizeSchema(vector.schema), vector.value);
    } catch {
      /* The former implementation applied these same admission checks. */
    }
    assert.equal(actual, former, name);
    assert.equal(actual, vector.valid, name);
  }
});
