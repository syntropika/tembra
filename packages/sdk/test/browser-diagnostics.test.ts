import { expect, test } from "bun:test";

import { browserErrors } from "../src/browser/diagnostics.js";

test("browser diagnostics retain detached structured errors", () => {
  const errors = browserErrors(
    '[{"code":"invalid","reason":"javascript","message":"observer failed","details":{"cause":[1]}}]',
  );
  expect(errors).toEqual([
    { code: "invalid", reason: "javascript", message: "observer failed", details: { cause: [1] } },
  ]);
  expect(Object.isFrozen(errors)).toBe(true);
  expect(Object.isFrozen(errors[0])).toBe(true);
  expect(Object.isFrozen(errors[0]?.details)).toBe(true);
});

test("browser diagnostics refuse malformed error envelopes", () => {
  for (const value of [
    "null",
    "{}",
    "[null]",
    '[{"code":"invalid","reason":"javascript","message":"missing details"}]',
  ]) {
    expect(() => browserErrors(value)).toThrow("browser error");
  }
});
