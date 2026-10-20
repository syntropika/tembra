import { test } from "bun:test";
import assert from "node:assert/strict";

import { Type, defineFacet, local, method } from "../src/index.js";

const Counter = local("executor.counter", {
  version: 1,
  input: Type.Object({ name: Type.String() }),
  members: { read: method(undefined, Type.Integer()) },
});

test("defineFacet owns immutable copied metadata without running or freezing setup", () => {
  let calls = 0;
  const setup = (): undefined => {
    calls += 1;
  };
  const uses = [Counter, Counter];
  const source = { name: " counter facet ", uses, setup };
  const facet = defineFacet(source);
  uses.pop();
  source.name = "changed";
  assert.equal(calls, 0);
  assert.equal(facet.name, " counter facet ");
  assert.equal(facet.setup, setup);
  assert.notEqual(facet.uses, uses);
  assert.deepEqual(facet.uses, [Counter, Counter]);
  assert.equal(facet.uses[0], Counter);
  assert.equal(Object.isFrozen(facet), true);
  assert.equal(Object.isFrozen(facet.uses), true);
  assert.equal(Object.isFrozen(setup), false);
  assert.equal(Reflect.set(facet, "name", "changed"), false);
  assert.equal(facet.name, " counter facet ");
});

test("defineFacet accepts an empty uses list and validates metadata without side effects", () => {
  let entries = 0;
  const setup = (): undefined => {
    entries += 1;
  };
  assert.deepEqual(defineFacet({ name: "empty", uses: [], setup }).uses, []);
  for (const name of ["", "bad\nname", "bad\u0085name"]) {
    assert.throws(() => defineFacet({ name, uses: [], setup }), /facet_name/u);
  }
  const invalidUses = { name: "invalid", uses: null, setup };
  assert.throws(() => {
    Reflect.apply(defineFacet, undefined, [invalidUses]);
  }, /facet_uses/u);
  const invalidSetup = { name: "invalid", uses: [], setup: null };
  assert.throws(() => {
    Reflect.apply(defineFacet, undefined, [invalidSetup]);
  }, /facet_setup/u);
  assert.equal(entries, 0);
});
