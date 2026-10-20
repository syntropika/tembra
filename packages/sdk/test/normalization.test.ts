import { test } from "bun:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";

import { ContractError } from "../src/error.js";
import { canonicalIdentity, normalizeInput } from "../src/index.js";

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function assertInputRefusal(operation: () => unknown): void {
  assert.throws(operation, (failure: unknown) => {
    assert.ok(failure instanceof ContractError, "expected a typed ContractError");
    assert.equal(failure.reason, "input");
    return true;
  });
}

const fixture: unknown = JSON.parse(
  await readFile(
    new URL("../../../testing/fixtures/contracts/normalization-v1.json", import.meta.url),
    "utf-8",
  ),
);
assert.ok(isRecord(fixture));
assert.equal(fixture.version, 1);
assert.ok(Array.isArray(fixture.groups) && fixture.groups.length > 0);
const groups: unknown[] = fixture.groups;

for (const group of groups) {
  assert.ok(isRecord(group));
  assert.ok(typeof group.name === "string");
  assert.ok("schema" in group);
  assert.ok(Array.isArray(group.cases) && group.cases.length > 0);
  const cases: unknown[] = group.cases;
  for (const vector of cases) {
    assert.ok(isRecord(vector));
    assert.ok(typeof vector.name === "string");
    assert.ok("input" in vector);
    const { expected } = vector;
    assert.ok(isRecord(expected));
    assert.equal(typeof expected.accepted, "boolean");

    test(`${group.name}: ${vector.name}`, () => {
      const before = structuredClone(vector.input);
      if (expected.accepted === true) {
        assert.ok("normalized" in expected);
        assert.equal(typeof expected.identity, "string");
        assert.deepEqual(normalizeInput(group.schema, vector.input), expected.normalized);
        assert.deepEqual(vector.input, before, "normalization leaves input unchanged");
        assert.equal(canonicalIdentity(group.schema, vector.input), expected.identity);
      } else {
        assertInputRefusal(() => normalizeInput(group.schema, vector.input));
        assert.deepEqual(vector.input, before, "refused normalization leaves input unchanged");
        assertInputRefusal(() => canonicalIdentity(group.schema, vector.input));
      }
      assert.deepEqual(vector.input, before, "identity calculation leaves input unchanged");
    });
  }
}
