import { test } from "bun:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";

import {
  Type,
  canonicalJson,
  descriptorBytes,
  fromDescriptor,
  local,
  normalizeSchema,
} from "../src/index.js";

type DescriptorVector = {
  name: string;
  descriptor: unknown;
  canonical: string;
  fingerprint: string;
};
type ReviewCorpus = {
  schemaRejections: {
    name: string;
    stage: string;
    schema: unknown;
    reason: string;
    path: string;
  }[];
  digests: DescriptorVector[];
  canonical: { name: string; value: unknown; bytes: string }[];
  references: DescriptorVector & { schema: unknown };
};
// SAFETY: This checked-in versioned fixture has a shared Rust/SDK schema; cases below validate each descriptor and value.
// oxlint-disable typescript/no-unsafe-type-assertion -- Read this one authored fixture initializer using its documented shared schema.
const corpus = JSON.parse(
  await readFile(
    new URL("../../../testing/fixtures/contracts/review-v1.json", import.meta.url),
    "utf-8",
  ),
) as ReviewCorpus;
// oxlint-enable typescript/no-unsafe-type-assertion

test("shared negative source fixtures preserve rejection stage, reason, and structured schema path", () => {
  for (const vector of corpus.schemaRejections) {
    assert.equal(vector.stage, "schema", vector.name);
    assert.throws(
      () => normalizeSchema(vector.schema),
      { reason: vector.reason, path: vector.path },
      vector.name,
    );
  }
});

test("each descriptor mutation changes its verified digest and schema array ordering is retained", () => {
  const seen = new Set<string>();
  for (const vector of corpus.digests) {
    const token = fromDescriptor(vector.descriptor);
    assert.equal(descriptorBytes(token), vector.canonical, vector.name);
    assert.equal(token.descriptor.fingerprint, vector.fingerprint, vector.name);
    assert.equal(seen.has(vector.fingerprint), false, vector.name);
    seen.add(vector.fingerprint);
  }
  assert.equal(seen.size, 7);
  for (const vector of corpus.canonical) {
    assert.equal(canonicalJson(vector.value), vector.bytes, vector.name);
  }
});

test("independent TypeBox repeated references match the Serde/Schemars-origin schema and descriptor", () => {
  const item = Type.Object({ label: Type.String() }, { title: "ReferenceItem" });
  const input = Type.Object(
    {
      left: Type.Ref("#/$defs/ReferenceItem"),
      right: Type.Ref("#/$defs/ReferenceItem"),
    },
    { title: "ReferenceInput", $defs: { ReferenceItem: item } },
  );
  const authored = local("fixture.refs", { version: 1, input, members: {} });
  const expected = corpus.references;
  const decoded = fromDescriptor(expected.descriptor);
  assert.deepEqual(normalizeSchema(expected.schema), authored.descriptor.input);
  assert.equal(descriptorBytes(authored), expected.canonical);
  assert.equal(authored.descriptor.fingerprint, expected.fingerprint);
  assert.deepEqual(authored.descriptor, decoded.descriptor);
});
