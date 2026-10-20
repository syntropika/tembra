import { test } from "bun:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";

import { Value as TypeBoxValue } from "typebox/value";

import {
  Type,
  canonicalIdentity,
  canonicalJson,
  cleanArguments,
  descriptorBytes,
  fromDescriptor,
  hostFamily,
  hostNamed,
  local,
  method,
  native,
  normalizeInput,
  normalizeSchema,
  opaque,
  parsePortableJson,
  remote,
  validateValue,
} from "../src/index.js";

type Corpus = {
  descriptors: {
    name: string;
    descriptor: unknown;
    canonical: string;
    fingerprint: string;
  }[];
  validation: {
    name: string;
    schema: unknown;
    value: unknown;
    valid: boolean;
  }[];
  normalization: {
    name: string;
    schema: unknown;
    input: unknown;
    normalized: unknown;
    identity: string;
  }[];
  canonical: { name: string; value: unknown; bytes: string }[];
  rawJson: { name: string; text: string; valid: boolean }[];
};
// SAFETY: This checked-in versioned fixture has a shared Rust/SDK schema; cases below validate each descriptor and value.
// oxlint-disable typescript/no-unsafe-type-assertion -- Read this one authored fixture initializer using its documented shared schema.
const corpus = JSON.parse(
  await readFile(new URL("../../../fixtures/contracts/v1.json", import.meta.url), "utf-8"),
) as Corpus;
// oxlint-enable typescript/no-unsafe-type-assertion

const firstFixture = corpus.descriptors.at(0);
assert.ok(firstFixture !== undefined);
const firstDescriptor = fromDescriptor(firstFixture.descriptor).descriptor;

test("shared Rust/TypeScript descriptor and canonical vectors", () => {
  for (const vector of corpus.descriptors) {
    const contract = fromDescriptor(vector.descriptor);
    assert.equal(descriptorBytes(contract), vector.canonical, vector.name);
    assert.equal(contract.descriptor.fingerprint, vector.fingerprint, vector.name);
  }
  for (const vector of corpus.canonical) {
    assert.equal(canonicalJson(vector.value), vector.bytes, vector.name);
  }
});

test("shared Rust/TypeScript runtime validation vectors", () => {
  for (const vector of corpus.validation) {
    let accepted = false;
    try {
      accepted = validateValue(vector.schema, vector.value);
    } catch {
      /* Invalid portable data is a rejection. */
    }
    assert.equal(accepted, vector.valid, vector.name);
  }
});

test("shared Rust/TypeScript acquisition and raw JSON vectors", () => {
  for (const vector of corpus.normalization) {
    assert.deepEqual(normalizeInput(vector.schema, vector.input), vector.normalized, vector.name);
    assert.equal(canonicalIdentity(vector.schema, vector.input), vector.identity, vector.name);
  }
  for (const vector of corpus.rawJson) {
    let accepted = true;
    try {
      parsePortableJson(vector.text);
    } catch {
      accepted = false;
    }
    assert.equal(accepted, vector.valid, vector.name);
  }
});

test("TypeBox authoring emits the shared normalized descriptor without library metadata", () => {
  const Counter = remote("fixture.counter", {
    version: 1,
    input: Type.Object({ name: Type.String() }),
    host: hostNamed("service"),
    variants: {
      default: {
        state: { count: Type.Integer() },
        methods: {
          increment: method(Type.Object({ by: Type.Integer() }), Type.Integer()),
          read: method(undefined, Type.Integer()),
          touch: method(),
          nullable: method(Type.Null(), Type.Null()),
        },
      },
    },
  });
  assert.equal(Counter.descriptor.fingerprint, firstFixture.fingerprint);
  assert.equal(Counter.resolveHost({ name: "counter" }), "service");
  const Family = remote("fixture.family", {
    version: 1,
    input: Type.Object({ name: Type.String() }),
    host: hostFamily("service:", "name"),
    variants: { main: {} },
  });
  assert.equal(Family.resolveHost({ name: "a" }), "service:a");
  assert.throws(() => Family.resolveHost({ name: "" }), /placement/u);
  assert.throws(
    () =>
      remote("invalid.family", {
        version: 1,
        input: Type.Object({ name: Type.Optional(Type.String()) }),
        host: hostFamily("service:", "name"),
        variants: { main: {} },
      }),
    /placement/u,
  );
  const Native = local("fixture.local", {
    version: 1,
    input: Type.Object({}),
    members: {
      native: method(native<URL>("url@1"), native<URL>("url@1")),
      handle: opaque<URL>("url@1"),
    },
  });
  assert.deepEqual(Native.descriptor.variants[""]?.native, {
    kind: "method",
    args: { native: "url@1" },
    result: { native: "url@1" },
  });
});

test("independently authored TypeBox matches the Serde/Schemars-origin descriptor", async () => {
  const rust: unknown = JSON.parse(
    await readFile(
      new URL("../../../fixtures/contracts/typed-rust.json", import.meta.url),
      "utf-8",
    ),
  );
  const int32 = { minimum: -2_147_483_648, maximum: 2_147_483_647 };
  const contract = remote("fixture.typed-counter", {
    version: 1,
    input: Type.Object({ name: Type.String() }, { title: "CounterInput" }),
    host: hostNamed("service"),
    variants: {
      default: {
        state: { count: Type.Integer({ ...int32, title: "int32" }) },
        methods: {
          increment: method(
            Type.Object({ by: Type.Integer(int32) }, { title: "Increment" }),
            Type.Integer({ ...int32, title: "int32" }),
          ),
          read: method(undefined, Type.Integer({ ...int32, title: "int32" })),
          touch: method(),
        },
      },
    },
  });
  assert.equal(canonicalJson(contract.descriptor), canonicalJson(rust));
  assert.equal(
    contract.descriptor.fingerprint,
    "26c3d3f912aa233a48750cc691248a92e3b545f94f97bf1f94cf035c282547b8",
  );
});

test("normalization inlines supported references, preserves trivalent cleaning, and rejects unsupported forms", () => {
  for (const value of [false, true]) {
    const source = {
      $defs: { leaf: value, alias: { $ref: "#/$defs/leaf" } },
      $ref: "#/$defs/alias",
    };
    assert.deepEqual(normalizeSchema(source), value ? {} : false);
    assert.throws(
      () => normalizeSchema({ ...source, description: "Annotated alias" }),
      /annotation siblings on a boolean reference/u,
    );
  }
  const pointerSource = {
    $defs: { choices: { anyOf: [{ type: "string" }, { type: "integer" }] } },
    $ref: "#/$defs/choices/anyOf/0",
  };
  assert.deepEqual(normalizeSchema(pointerSource), { type: "string" });
  for (const index of ["00", "-", "2", "9007199254740992"]) {
    assert.throws(
      () =>
        normalizeSchema({
          ...pointerSource,
          $ref: `#/$defs/choices/anyOf/${index}`,
        }),
      /unresolved local reference/u,
    );
  }
  assert.deepEqual(
    normalizeSchema({
      $defs: { "a/b": { type: "integer" } },
      type: "array",
      items: { $ref: "#/$defs/a~1b", title: "Number" },
    }),
    { type: "array", items: { type: "integer", title: "Number" } },
  );
  assert.deepEqual(
    normalizeSchema({
      $defs: { value: { type: "string" } },
      $ref: "#/$defs/value",
    }),
    { type: "string" },
  );
  assert.deepEqual(normalizeSchema({ type: ["number", "null"] }), {
    anyOf: [{ type: "number" }, { type: "null" }],
  });
  assert.deepEqual(normalizeSchema({ type: "array" }), {
    type: "array",
    items: {},
  });
  assert.throws(
    () =>
      normalizeSchema({
        $defs: { value: { $ref: "#/$defs/value" } },
        $ref: "#/$defs/value",
      }),
    /recursive_schema/u,
  );
  assert.throws(
    () =>
      normalizeSchema({
        $defs: { value: { type: "integer" } },
        $ref: "#/$defs/value",
        minimum: 2,
      }),
    /annotation siblings/u,
  );
  assert.throws(
    () =>
      normalizeSchema({
        $defs: { unused: { $ref: "#/$defs/missing" } },
        type: "number",
      }),
    /unresolved/u,
  );
  for (const schema of [
    { type: "string", pattern: "x" },
    { type: "array", items: [] },
    { not: {} },
    { $schema: "http://json-schema.org/draft-07/schema#" },
  ]) {
    assert.throws(() => normalizeSchema(schema));
  }
  assert.throws(
    () =>
      normalizeInput(
        {
          anyOf: [
            { type: "object", properties: { a: { type: "string" } } },
            { type: "object", properties: { b: { type: "string" } } },
          ],
        },
        {},
      ),
    /ambiguous_normalization/u,
  );
  assert.deepEqual(
    cleanArguments(
      {
        type: "object",
        properties: { count: { type: "integer", default: 2 } },
      },
      { extra: 1 },
    ),
    {},
  );
  assert.deepEqual(cleanArguments({ type: "object", additionalProperties: true }, { extra: 1 }), {
    extra: 1,
  });
  assert.deepEqual(cleanArguments({ additionalProperties: false }, { extra: 1 }), {});
  assert.deepEqual(normalizeInput({}, { extra: 1 }), { extra: 1 });
  assert.throws(
    () =>
      fromDescriptor({
        ...firstDescriptor,
        ignored: true,
      }),
    /unknown field/u,
  );
  assert.throws(
    () =>
      fromDescriptor({
        ...firstDescriptor,
        fingerprint: "0".repeat(64),
      }),
    /digest mismatch/u,
  );
});

test("nominal identifiers use Unicode White_Space consistently with Rust", () => {
  assert.throws(() => native("name\u0085@1"), /native_id/u);
  assert.deepEqual(native("name\uFEFF@1"), { native: "name\uFEFF@1" });
  assert.throws(() => native("name@\u0661"), /native_id/u);
});

test("strict ingestion rejects duplicate decoded keys, unsafe lexical numbers, accessors, sparse arrays, and surrogates", () => {
  for (const text of [
    '{"x":1,"\\u0078":2}',
    "9007199254740993",
    "900719925474099300e-2",
    "9007199254740991.1",
    "1e999",
    "1e-999",
    "[1,]",
    "/*x*/{}",
    '"\\udc00"',
  ]) {
    assert.throws(() => parsePortableJson(text), text);
  }
  assert.equal(parsePortableJson("900719925474099100e-2"), Number.MAX_SAFE_INTEGER);
  const sparse: unknown[] = [];
  sparse.length = 1;
  for (const value of [
    sparse,
    { x: undefined },
    new Date(),
    {
      get x() {
        throw new Error("getter must not execute");
      },
    },
    Symbol("x"),
    "\uD800",
  ]) {
    assert.throws(() => canonicalJson(value));
  }
  assert.equal(
    canonicalJson(parsePortableJson('{"__proto__":{"ok":true}}')),
    '{"__proto__":{"ok":true}}',
  );
  assert.equal(
    validateValue({ anyOf: [{ type: "string", minLength: 2 }, { type: "null" }] }, "e\u0301"),
    true,
  );
  assert.throws(() => parsePortableJson(`${"[".repeat(200)}0${"]".repeat(200)}`), /depth/u);
  assert.throws(
    () => normalizeSchema({ type: "array", items: { type: "string" } }, { maxDepth: 0 }),
    /schema_budget/u,
  );
  assert.throws(
    () => normalizeSchema({ type: "array", items: { type: "string" } }, { maxNodes: 1 }),
    /schema_budget/u,
  );
});

test("sibling union transformations reject before erasing acquisition or argument fields", () => {
  const schema = {
    type: "object",
    properties: { a: { type: "string" } },
    anyOf: [{ type: "object" }],
  };
  for (const value of [{ a: "x" }, { a: "y" }]) {
    assert.throws(() => normalizeInput(schema, value), {
      reason: "ambiguous_normalization",
      path: "/anyOf",
    });
    assert.throws(() => cleanArguments(schema, value), {
      reason: "ambiguous_normalization",
      path: "/anyOf",
    });
    assert.deepEqual(Object.keys(value), ["a"]);
  }
  assert.deepEqual(
    normalizeInput(
      { type: "object", properties: { a: { type: "string" } }, anyOf: [{}] },
      { a: "x" },
    ),
    { a: "x" },
  );
});

test("nullable object and array recipes choose the non-null branch without losing defaults or null", () => {
  const schema = {
    type: "object",
    properties: {
      record: {
        type: ["null", "object"],
        properties: { a: { type: "string", default: "fallback" } },
      },
      entries: {
        type: ["array", "null"],
        items: {
          type: "object",
          properties: { n: { type: "integer", default: 2 } },
        },
      },
    },
  };
  assert.deepEqual(normalizeInput(schema, { record: null, entries: null }), {
    record: null,
    entries: null,
  });
  assert.deepEqual(normalizeInput(schema, { record: {}, entries: [{}] }), {
    record: { a: "fallback" },
    entries: [{ n: 2 }],
  });
  assert.equal(
    canonicalIdentity(schema, { record: {}, entries: [{}] }),
    canonicalIdentity(schema, {
      record: { a: "fallback" },
      entries: [{ n: 2 }],
    }),
  );
  assert.deepEqual(cleanArguments(schema, { record: { extra: 1 }, entries: [{ extra: 2 }] }), {
    record: {},
    entries: [{}],
  });
});

test("TypeBox schema-map names are distinct from annotation data", () => {
  const input = Type.Object({
    default: Type.String(),
    const: Type.String(),
    enum: Type.String(),
    examples: Type.String(),
  });
  const contract = local("review.annotation-names", {
    version: 1,
    input,
    members: {},
  });
  const value = { default: "a", const: "b", enum: "c", examples: "d" };
  assert.deepEqual(contract.normalizeInput(value), value);
  for (const name of Object.keys(value)) {
    assert.deepEqual(
      normalizeSchema({
        $defs: { [name]: Type.String() },
        $ref: `#/$defs/${name}`,
      }),
      { type: "string" },
    );
  }
  assert.throws(() => normalizeSchema(Type.String({ default: Type.String() })), /portable/u);
});

test("placement resolution validates input even for named hosts and local contracts", () => {
  const input = Type.Object({ name: Type.String() });
  const named = remote("review.named", {
    version: 1,
    input,
    host: hostNamed("service"),
    variants: { main: {} },
  });
  const own = local("review.local", { version: 1, input, members: {} });
  assert.throws(() => named.resolveHost({ name: 1 }));
  assert.throws(() => own.resolveHost({ name: 1 }));
  assert.equal(named.resolveHost({ name: "x" }), "service");
  assert.equal(own.resolveHost({ name: "x" }), undefined);
});

test("portable equality treats negative zero as zero for constants, enums, and unique items", () => {
  // The selected upstream checker hashes numeric storage bits for uniqueness.
  assert.equal(TypeBoxValue.Check({ type: "array", uniqueItems: true }, [-0, 0]), true);
  assert.equal(validateValue({ type: "array", uniqueItems: true }, [-0, 0]), false);
  assert.equal(validateValue({ type: "array", uniqueItems: true }, [{ n: -0 }, { n: 0 }]), false);
  assert.equal(validateValue({ type: "array", uniqueItems: true }, [-0, 1]), true);
  for (const schema of [{ const: 0 }, { const: -0 }, { enum: [0] }, { enum: [-0] }]) {
    assert.equal(validateValue(schema, 0), true);
    assert.equal(validateValue(schema, -0), true);
  }
  assert.throws(() => normalizeSchema({ enum: [-0, 0] }), /unique portable/u);
});

test("strict depth permits exactly 128 containers and safe descriptor integers accept float spellings", () => {
  for (const scalar of ["", "0"]) {
    const boundary = "[".repeat(128) + scalar + "]".repeat(128);
    assert.equal(canonicalJson(parsePortableJson(boundary)), boundary);
    const tooDeep = "[".repeat(129) + scalar + "]".repeat(129);
    assert.throws(() => parsePortableJson(tooDeep), { reason: "depth" });
    assert.throws(() => canonicalJson(JSON.parse(tooDeep)), {
      reason: "depth",
    });
  }
  const bytes = canonicalJson(firstDescriptor);
  const floatSpelled = bytes
    .replace('"descriptorVersion":1', '"descriptorVersion":1e0')
    .replace('"version":1', '"version":1.0');
  const contract = fromDescriptor(parsePortableJson(floatSpelled));
  assert.equal(contract.descriptor.fingerprint, firstFixture.fingerprint);
});
