import { test } from "bun:test";
import assert from "node:assert/strict";

import { TextEncoder as ServerEncoder } from "../../src/tooling/encoding.js";

test("server UTF-8 encoder matches platform scalar replacement and partial writes", () => {
  // Compare complete scalar prefixes independently of the platform encodeInto implementation.
  const expected = new TextEncoder();
  const actual = new ServerEncoder();
  for (const text of [
    "",
    "ascii",
    "caf\u00E9",
    "\u4E2D\u6587",
    "\uD83D\uDE80",
    "a\uD800b\uDC00c",
    "\uD800\uD800\uDC00",
  ]) {
    assert.deepEqual(actual.encode(text), expected.encode(text));
    for (let length = 0; length <= expected.encode(text).length + 2; length += 1) {
      const left = new Uint8Array(length).fill(42);
      const right = new Uint8Array(length).fill(42);
      let prefix = "";
      for (const scalar of text) {
        if (expected.encode(prefix + scalar).length > length) {
          break;
        }
        prefix += scalar;
      }
      const bytes = expected.encode(prefix);
      right.set(bytes);
      assert.deepEqual(actual.encodeInto(text, left), {
        read: prefix.length,
        written: bytes.length,
      });
      assert.deepEqual(left, right);
    }
  }
  assert.equal(actual.encoding, "utf-8");
});
