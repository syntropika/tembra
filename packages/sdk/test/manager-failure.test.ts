import { test } from "bun:test";
import assert from "node:assert/strict";

import { connectionFailure, failureMessage } from "../src/manager/failure.js";

test.each([false, true])(
  "manager preserves structured core closure diagnostics across object/string platform wire (json=%s)",
  (json) => {
    const original = {
      code: "unavailable",
      reason: "liveness",
      message: "peer stopped responding",
      details: { target: "child" },
    };
    const failure = connectionFailure(json ? JSON.stringify(original) : original);
    assert.equal(failure.message, original.message);
    assert.ok("code" in failure && failure.code === "unavailable");
    assert.ok("reason" in failure && failure.reason === "liveness");
    assert.deepEqual(failure.cause, original);
    assert.ok("details" in failure);
    assert.deepEqual(failure.details, original.details);
    const forwarded = connectionFailure(failureMessage(failure));
    assert.ok("reason" in forwarded && forwarded.reason === "liveness");
    assert.ok("code" in forwarded && forwarded.code === "unavailable");
    assert.ok("details" in forwarded);
    assert.deepEqual(forwarded.details, original.details);
  },
);
test("manager preserves an existing platform Error identity and unstructured close cause", () => {
  const original = new Error("worker failed", { cause: new Error("platform original") });
  assert.equal(connectionFailure(original), original);
  const unstructured = connectionFailure("transport closed");
  assert.equal(unstructured.cause, "transport closed");
});
