import { test } from "bun:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";

import {
  evaluateEsmArtifact,
  inspectEsmSource,
  realmDiagnostics,
  validateEsmArtifact,
} from "../src/browser/artifact.js";
import type { EsmArtifactV1 } from "../src/index.js";

function present<T>(value: T | null | undefined): T {
  assert.ok(value !== null && value !== undefined, "The fixture issued its expected value");
  return value;
}
function artifact(source = "export default {};"): EsmArtifactV1 {
  return {
    format: "tembra-esm-v1",
    identity: "test",
    source,
    sha256: createHash("sha256").update(source).digest("hex"),
    exports: ["default"],
  };
}
test("actual pinned lexer executes the shared native/browser closed-source corpus", async () => {
  const corpus =
    /* SAFETY: esm-v1.json is the versioned repository lexical corpus; these case fields match the corpus consumed by the native loader tests. */
    /* oxlint-disable typescript/no-unsafe-type-assertion -- The versioned corpus has the fixed case schema consumed below. */ JSON.parse(
      await readFile(
        new URL("../../../testing/fixtures/executor/esm-v1.json", import.meta.url),
        "utf-8",
      ),
    ) as {
      cases: {
        id: string;
        source: string;
        exports: string[];
        accepted: boolean;
      }[];
    };
  /* oxlint-enable typescript/no-unsafe-type-assertion */
  for (const item of corpus.cases) {
    let accepted = true;
    try {
      // oxlint-disable-next-line no-await-in-loop -- Each lifecycle observation depends on the previous step settling.
      await inspectEsmSource(item.source, item.exports);
    } catch {
      accepted = false;
    }
    assert.equal(accepted, item.accepted, item.id);
  }
});
test("artifact validation rejects hash, identity, exports and Unicode errors before admission", async () => {
  const valid = artifact();
  assert.equal(await validateEsmArtifact(valid, "default"), Buffer.byteLength(valid.source));
  await assert.rejects(
    validateEsmArtifact({ ...valid, sha256: "0".repeat(64) }, "default"),
    /hash mismatch/u,
  );
  for (const identity of ["", "bad\nidentity", "bad\u0085identity"]) {
    // oxlint-disable-next-line no-await-in-loop -- Each lifecycle observation depends on the previous step settling.
    await assert.rejects(validateEsmArtifact({ ...valid, identity }, "default"), /metadata/u);
  }
  for (const exports of [[""], ["default", "default"], ["missing"]]) {
    // oxlint-disable-next-line no-await-in-loop -- Each lifecycle observation depends on the previous step settling.
    await assert.rejects(validateEsmArtifact({ ...valid, exports }, "default"));
  }
  await assert.rejects(
    validateEsmArtifact({ ...valid, source: "\uD800" }, "default"),
    /surrogate/u,
  );
  await assert.rejects(
    validateEsmArtifact(artifact("export const other = 1;"), "default"),
    /exports/u,
  );
});
test("private source copy prevents caller mutation during asynchronous hashing", async () => {
  const original = artifact();
  const mutable = { ...original };
  const pending = validateEsmArtifact(mutable, "default");
  mutable.source = "import('outside'); export default {};";
  mutable.sha256 = "0".repeat(64);
  assert.equal(await pending, Buffer.byteLength(original.source));
});
test("metadata accessors are refused without executing their getters", async () => {
  let reads = 0;
  const value = {
    ...artifact(),
    get source() {
      reads += 1;
      return "export default {};";
    },
  };
  await assert.rejects(validateEsmArtifact(value, "default"), /accessors/u);
  assert.equal(reads, 0);
});
test("refused Rust reservation changes no realm counter and executes no module", async () => {
  const before = realmDiagnostics();
  await assert.rejects(
    evaluateEsmArtifact(artifact(), "default", () => {
      throw new Error("reservation denied");
    }),
    /reservation denied/u,
  );
  assert.deepEqual(realmDiagnostics(), before);
});
test("changed lexer dependencies refuse closed-source inspection without calling the replacement", async () => {
  const original = present(Object.getOwnPropertyDescriptor(String.prototype, "charCodeAt"));
  let calls = 0;
  let pending: Promise<void>;
  try {
    // oxlint-disable-next-line no-extend-native -- This refusal test deliberately mutates and restores the actual intrinsic descriptor.
    Object.defineProperty(String.prototype, "charCodeAt", {
      ...original,
      value() {
        calls += 1;
        return 0;
      },
    });
    pending = inspectEsmSource("import('outside'); export default {};", ["default"]);
  } finally {
    // oxlint-disable-next-line no-extend-native -- This refusal test deliberately mutates and restores the actual intrinsic descriptor.
    Object.defineProperty(String.prototype, "charCodeAt", original);
  }
  await assert.rejects(present(pending), { reason: "lexer_intrinsics" });
  assert.equal(calls, 0);
  await inspectEsmSource("export default {};", ["default"]);
});
