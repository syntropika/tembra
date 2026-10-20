import { expect, test } from "bun:test";

import { assertQuiescent, deferredVoid, defined, structuredError } from "../src/assertions.ts";

test("physical settlement gate remains pending until its owner releases it", async () => {
  const gate = deferredVoid();
  let settled = false;
  const observed = gate.promise.then(() => {
    settled = true;
  });
  await Promise.resolve();
  expect(settled).toBe(false);
  gate.resolve();
  await observed;
  expect(settled).toBe(true);
});

test("diagnostics identify all outstanding managed ownership counters", () => {
  expect(() => {
    assertQuiescent({ callbacks: 0, jobs: 0 });
  }).not.toThrow();
  expect(() => {
    assertQuiescent({ callbacks: 2, jobs: 1 });
  }).toThrow("callbacks=2, jobs=1");
  expect(() => {
    assertQuiescent({ jobs: Number.NaN });
  }).toThrow("jobs=NaN");
});

test("observations and structured refusals require real evidence", () => {
  expect(defined(false)).toBe(false);
  expect(defined(null)).toBeNull();
  expect(() => {
    defined(undefined);
  }).toThrow();
  expect(() => {
    structuredError(new Error("missing diagnostics"));
  }).toThrow();
  structuredError(
    Object.assign(new Error("refused"), { reason: "test", detailsJson: "{}", details: {} }),
  );
});
