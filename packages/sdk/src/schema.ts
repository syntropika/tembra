import { ContractError } from "./error.js";
import { assertPortable, canonicalJson, clonePortable } from "./portable.js";
import type { JsonValue } from "./portable.js";

/** Supported portable schema representation; use normalizeSchema to check authoring input. */
export type Schema = boolean | Record<string, JsonValue>;
const annotations = new Set([
  "title",
  "description",
  "default",
  "examples",
  "$comment",
  "readOnly",
  "writeOnly",
  "deprecated",
]);
const keywords = new Set([
  "type",
  "const",
  "enum",
  "properties",
  "required",
  "additionalProperties",
  "items",
  "anyOf",
  "minimum",
  "maximum",
  "exclusiveMinimum",
  "exclusiveMaximum",
  "minLength",
  "maxLength",
  "minItems",
  "maxItems",
  "minProperties",
  "maxProperties",
  "uniqueItems",
  ...annotations,
]);
const kinds = new Set(["null", "boolean", "integer", "number", "string", "array", "object"]);

function assertObject(value: unknown, reason: string): asserts value is Record<string, unknown> {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    throw new ContractError(reason, "expected object");
  }
}
export function object(value: unknown, reason = "schema"): Record<string, unknown> {
  assertObject(value, reason);
  return value;
}
function jsonObject(value: JsonValue | undefined, reason = "schema"): Record<string, JsonValue> {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    throw new ContractError(reason, "expected object");
  }
  return value;
}
function schemaChild(value: JsonValue | undefined): Schema {
  return typeof value === "boolean" ? value : jsonObject(value);
}

function schemaChildProperty(
  source: Record<string, JsonValue> | JsonValue[],
  key: string | number,
): JsonValue {
  const value = Array.isArray(source) ? source[Number(key)] : source[key];
  if (value === undefined) {
    throw new ContractError("schema", "missing normalized value");
  }
  return value;
}

/** Authoring-only TypeBox metadata is not part of the portable schema. */
function schemaData(
  source: unknown,
  depth = 0,
  active = new Set<object>(),
  context: "schema" | "map" = "schema",
): JsonValue {
  if (depth > 128) {
    throw new ContractError("depth", "schema source nesting limit exceeded");
  }
  if (source === null || typeof source !== "object") {
    assertPortable(source);
    return source;
  }
  if (active.has(source)) {
    throw new ContractError("schema", "cyclic source object; use acyclic $defs references");
  }
  const prototype: unknown = Object.getPrototypeOf(source);
  if (
    Array.isArray(source)
      ? prototype !== Array.prototype
      : prototype !== Object.prototype && prototype !== null
  ) {
    throw new ContractError("schema", "schema source must be plain data");
  }
  active.add(source);
  const result = Array.isArray(source)
    ? schemaArray(source, depth, active)
    : schemaRecord(source, depth, active, context);
  active.delete(source);
  assertPortable(result);
  return result;
}
function schemaArray(source: unknown[], depth: number, active: Set<object>): JsonValue[] {
  if (Reflect.ownKeys(source).length !== source.length + 1) {
    throw new ContractError("schema", "schema arrays must be dense without custom properties");
  }
  const result: JsonValue[] = [];
  for (let index = 0; index < source.length; index += 1) {
    const property = Object.getOwnPropertyDescriptor(source, String(index));
    if (property === undefined || !("value" in property) || property.enumerable !== true) {
      throw new ContractError("schema", "schema array has holes or accessors");
    }
    result.push(schemaData(property.value, depth + 1, active));
  }
  return result;
}
function schemaRecord(
  source: object,
  depth: number,
  active: Set<object>,
  context: "schema" | "map",
): Record<string, JsonValue> {
  const entries: [string, JsonValue][] = [];
  for (const key of Reflect.ownKeys(source)) {
    const property = Object.getOwnPropertyDescriptor(source, key);
    if (property === undefined) {
      throw new ContractError("schema", "missing source property");
    }
    if (
      typeof key === "string" &&
      property.enumerable !== true &&
      ["~kind", "~optional", "~readonly"].includes(key) &&
      "value" in property
    ) {
      continue;
    }
    if (typeof key !== "string" || property.enumerable !== true || !("value" in property)) {
      throw new ContractError("schema", "schema has symbols, hidden fields, or accessors");
    }
    entries.push([
      key,
      context === "schema" && ["default", "const", "enum", "examples"].includes(key)
        ? clonePortable(property.value)
        : schemaData(
            property.value,
            depth + 1,
            active,
            context === "schema" && ["properties", "$defs"].includes(key) ? "map" : "schema",
          ),
    ]);
  }
  return Object.fromEntries(entries);
}

/** Bounded schema compilation; limits are checked safe integers. */
export type CompileLimits = {
  /** Maximum schema nesting; defaults to 64 and may be zero. */
  readonly maxDepth: number;
  /** Maximum visited schema nodes; defaults to 16,384 and must be positive. */
  readonly maxNodes: number;
};

/**
 * Checks the supported schema subset and removes authoring-only metadata such as TypeBox markers.
 * Unsupported keywords, invalid schemas and exhausted budgets throw ContractError.
 */
export function normalizeSchema(source: unknown, limits: Partial<CompileLimits> = {}): Schema {
  const maxDepth = limits.maxDepth ?? 64;
  const maxNodes = limits.maxNodes ?? 16_384;
  if (
    !Number.isSafeInteger(maxDepth) ||
    maxDepth < 0 ||
    !Number.isSafeInteger(maxNodes) ||
    maxNodes <= 0
  ) {
    throw new ContractError(
      "schema_budget",
      "compiler limits must be nonnegative depth and positive node count",
    );
  }
  const root = schemaData(source);
  let nodes = 0;
  // Preserve boolean provenance through reference chains after true normalizes to {}.
  const booleanTrueResults = new WeakSet();
  function compile(nodeSource: JsonValue, path: string, depth: number, refs: Set<string>): Schema {
    nodes += 1;
    if (nodes > maxNodes || depth > maxDepth) {
      throw new ContractError("schema_budget", "schema expansion limit exceeded", path);
    }
    if (nodeSource === true) {
      const result = {};
      booleanTrueResults.add(result);
      return result;
    }
    if (nodeSource === false) {
      return false;
    }
    if (nodeSource === null || typeof nodeSource !== "object" || Array.isArray(nodeSource)) {
      throw new ContractError(
        "schema",
        "expected schema object or boolean",
        path.replace(/\/$/u, "") || "/",
      );
    }
    const input = jsonObject(nodeSource);
    if (
      input.$schema !== undefined &&
      input.$schema !== "https://json-schema.org/draft/2020-12/schema"
    ) {
      throw new ContractError("dialect", "only JSON Schema 2020-12 is supported", path);
    }
    if (input.$defs !== undefined) {
      for (const [key, value] of Object.entries(jsonObject(input.$defs))) {
        compile(value, `${path}$defs/${key}/`, depth + 1, refs);
      }
    }
    if (input.$ref !== undefined) {
      return compileReference(input, path, depth, refs);
    }
    const output = Object.fromEntries(
      Object.entries(input).filter(([key]) => key !== "$schema" && key !== "$defs"),
    );
    for (const key of Object.keys(output)) {
      if (!keywords.has(key)) {
        throw new ContractError(
          "unsupported_schema",
          `unsupported keyword ${key}`,
          `${path}${key.replaceAll("~", "~0").replaceAll("/", "~1")}`,
        );
      }
    }
    normalizeTypeUnion(output, path);
    validateMetadata(output, path);
    return compileChildren(output, path, depth, refs);
  }
  function compileReference(
    input: Record<string, JsonValue>,
    path: string,
    depth: number,
    refs: Set<string>,
  ): Schema {
    const reference = input.$ref;
    if (
      typeof reference !== "string" ||
      !reference.startsWith("#/$defs/") ||
      /~(?:[^01]|$)/u.test(reference)
    ) {
      throw new ContractError(
        "reference",
        "only valid local $defs JSON Pointers are supported",
        path,
      );
    }
    if (refs.has(reference)) {
      throw new ContractError("recursive_schema", "recursive schema reference", path);
    }
    for (const key of Object.keys(input)) {
      if (key !== "$ref" && key !== "$schema" && key !== "$defs" && !annotations.has(key)) {
        throw new ContractError("reference_sibling", "$ref permits annotation siblings only", path);
      }
    }
    let target: JsonValue | undefined = root;
    for (const part of reference.slice(2).split("/")) {
      const key = part.replaceAll("~1", "/").replaceAll("~0", "~");
      if (Array.isArray(target)) {
        const index = Number(key);
        if (
          !/^(?:0|[1-9][0-9]*)$/u.test(key) ||
          !Number.isSafeInteger(index) ||
          index >= target.length
        ) {
          throw new ContractError("reference", "unresolved local reference", path);
        }
        target = target[index];
        continue;
      }
      const node = jsonObject(target, "reference");
      if (!Object.hasOwn(node, key)) {
        throw new ContractError("reference", "unresolved local reference", path);
      }
      target = node[key];
    }
    if (target === undefined) {
      throw new ContractError("reference", "unresolved local reference", path);
    }
    const resolved = compile(target, `${path}$ref/`, depth + 1, new Set([...refs, reference]));
    const siblings = Object.fromEntries(
      Object.entries(input).filter(([key]) => annotations.has(key)),
    );
    if (!Object.keys(siblings).length) {
      return resolved;
    }
    if (typeof resolved === "boolean" || booleanTrueResults.has(resolved)) {
      throw new ContractError(
        "reference_sibling",
        "annotation siblings on a boolean reference are not representable",
        path,
      );
    }
    return compile({ ...resolved, ...siblings }, path, depth + 1, refs);
  }
  function compileChildren(
    output: Record<string, JsonValue>,
    path: string,
    depth: number,
    refs: Set<string>,
  ): Schema {
    if (output.properties !== undefined) {
      output.properties = Object.fromEntries(
        Object.entries(jsonObject(output.properties)).map(([key, value]) => [
          key,
          compile(value, `${path}properties/${key}/`, depth + 1, refs),
        ]),
      );
    }
    validateRequired(output, path);
    if (output.items !== undefined) {
      output.items = compile(output.items, `${path}items/`, depth + 1, refs);
    } else if (output.type === "array") {
      output.items = {};
    }
    if (output.anyOf !== undefined) {
      if (!Array.isArray(output.anyOf) || !output.anyOf.length) {
        throw new ContractError("schema", "anyOf must be nonempty", path);
      }
      output.anyOf = output.anyOf.map((branch, index) =>
        compile(branch, `${path}anyOf/${index}/`, depth + 1, refs),
      );
    }
    if (
      output.enum !== undefined &&
      (!Array.isArray(output.enum) ||
        !output.enum.length ||
        new Set(output.enum.map(canonicalJson)).size !== output.enum.length)
    ) {
      throw new ContractError("schema", "enum must contain unique portable values", path);
    }
    if (Object.hasOwn(output, "default") && !checkNormalized(output, output.default)) {
      throw new ContractError("schema", "default does not satisfy its schema", `${path}default`);
    }
    return output;
  }
  return compile(root, "/", 0, new Set());
}

function validateRequired(output: Record<string, JsonValue>, path: string): void {
  if (
    output.required !== undefined &&
    (!Array.isArray(output.required) ||
      output.required.some(
        (key) => typeof key !== "string" || !Object.hasOwn(object(output.properties ?? {}), key),
      ) ||
      new Set(output.required).size !== output.required.length)
  ) {
    throw new ContractError("schema", "required names must be unique declared properties", path);
  }
}
function typeKeywords(type: string): string[] {
  if (type === "object") {
    return ["properties", "required", "additionalProperties", "minProperties", "maxProperties"];
  }
  if (type === "array") {
    return ["items", "minItems", "maxItems", "uniqueItems"];
  }
  return [];
}
function normalizeTypeUnion(output: Record<string, JsonValue>, path: string): void {
  if (Array.isArray(output.type)) {
    if (
      !output.type.length ||
      output.anyOf !== undefined ||
      new Set(output.type).size !== output.type.length
    ) {
      throw new ContractError("schema", "empty or duplicate type union", path);
    }
    output.anyOf = output.type.map((type) => {
      if (typeof type !== "string" || !kinds.has(type)) {
        throw new ContractError("schema", "unknown type", `${path}type`);
      }
      const branch: Schema = { type };
      const keys = typeKeywords(type);
      for (const key of keys) {
        if (Object.hasOwn(output, key)) {
          branch[key] = schemaChildProperty(output, key);
        }
      }
      return branch;
    });
    for (const branch of output.anyOf) {
      for (const key of Object.keys(object(branch))) {
        if (key !== "type") {
          Reflect.deleteProperty(output, key);
        }
      }
    }
    delete output.type;
  }
}
function validateMetadata(output: Record<string, JsonValue>, path: string): void {
  if (output.type !== undefined && (typeof output.type !== "string" || !kinds.has(output.type))) {
    throw new ContractError("schema", "unknown type", `${path}type`);
  }
  for (const key of ["title", "description", "$comment"]) {
    if (output[key] !== undefined && typeof output[key] !== "string") {
      throw new ContractError("schema", `${key} must be text`, path);
    }
  }
  for (const key of [
    "readOnly",
    "writeOnly",
    "deprecated",
    "uniqueItems",
    "additionalProperties",
  ]) {
    if (output[key] !== undefined && typeof output[key] !== "boolean") {
      throw new ContractError("schema", `${key} must be boolean`, path);
    }
  }
  if (output.examples !== undefined && !Array.isArray(output.examples)) {
    throw new ContractError("schema", "examples must be an array", path);
  }
  for (const key of ["minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum"]) {
    if (output[key] !== undefined && typeof output[key] !== "number") {
      throw new ContractError("schema", `${key} must be numeric`, path);
    }
  }
  for (const key of [
    "minLength",
    "maxLength",
    "minItems",
    "maxItems",
    "minProperties",
    "maxProperties",
  ]) {
    if (
      output[key] !== undefined &&
      (typeof output[key] !== "number" || !Number.isSafeInteger(output[key]) || output[key] < 0)
    ) {
      throw new ContractError("schema", `${key} must be a nonnegative safe integer`, path);
    }
  }
}

function matchesType(kind: string, value: unknown): boolean {
  switch (kind) {
    case "null": {
      return value === null;
    }
    case "array": {
      return Array.isArray(value);
    }
    case "object": {
      return value !== null && typeof value === "object" && !Array.isArray(value);
    }
    case "integer": {
      return typeof value === "number" && Number.isInteger(value);
    }
    default: {
      return typeof value === kind;
    }
  }
}
function checkNumber(schema: Record<string, JsonValue>, value: number): boolean {
  return (
    !(typeof schema.minimum === "number" && value < schema.minimum) &&
    !(typeof schema.maximum === "number" && value > schema.maximum) &&
    !(typeof schema.exclusiveMinimum === "number" && value <= schema.exclusiveMinimum) &&
    !(typeof schema.exclusiveMaximum === "number" && value >= schema.exclusiveMaximum)
  );
}
function checkLength(
  schema: Record<string, JsonValue>,
  length: number,
  minimum: string,
  maximum: string,
): boolean {
  const lower = schema[minimum];
  const upper = schema[maximum];
  return (
    !(typeof lower === "number" && length < lower) && !(typeof upper === "number" && length > upper)
  );
}
function checkArray(schema: Record<string, JsonValue>, value: unknown[]): boolean {
  if (!checkLength(schema, value.length, "minItems", "maxItems")) {
    return false;
  }
  if (schema.uniqueItems === true && new Set(value.map(canonicalJson)).size !== value.length) {
    return false;
  }
  return (
    schema.items === undefined ||
    value.every((item) => checkNormalized(schemaChild(schema.items), item))
  );
}
function checkObject(schema: Record<string, JsonValue>, value: Record<string, unknown>): boolean {
  const keys = Object.keys(value);
  if (!checkLength(schema, keys.length, "minProperties", "maxProperties")) {
    return false;
  }
  if (
    Array.isArray(schema.required) &&
    schema.required.some((key) => typeof key !== "string" || !Object.hasOwn(value, key))
  ) {
    return false;
  }
  const properties = jsonObject(schema.properties ?? {});
  if (
    schema.additionalProperties === false &&
    keys.some((key) => !Object.hasOwn(properties, key))
  ) {
    return false;
  }
  return Object.entries(properties).every(
    ([key, property]) =>
      !Object.hasOwn(value, key) || checkNormalized(schemaChild(property), value[key]),
  );
}
function checkNormalized(schema: Schema, value: unknown): boolean {
  if (typeof schema === "boolean") {
    return schema;
  }
  // Normalization limits keywords to the same portable profile as the Rust checker.
  if (typeof schema.type === "string" && !matchesType(schema.type, value)) {
    return false;
  }
  if (Object.hasOwn(schema, "const") && canonicalJson(schema.const) !== canonicalJson(value)) {
    return false;
  }
  if (
    Array.isArray(schema.enum) &&
    !schema.enum.some((candidate) => canonicalJson(candidate) === canonicalJson(value))
  ) {
    return false;
  }
  if (
    Array.isArray(schema.anyOf) &&
    !schema.anyOf.some((branch) => checkNormalized(schemaChild(branch), value))
  ) {
    return false;
  }
  if (typeof value === "number") {
    return checkNumber(schema, value);
  }
  if (typeof value === "string") {
    // oxlint-disable-next-line typescript/no-misused-spread -- portable lengths count Unicode scalars, matching Rust chars(), rather than locale graphemes
    return checkLength(schema, [...value].length, "minLength", "maxLength");
  }
  if (Array.isArray(value)) {
    return checkArray(schema, value);
  }
  if (value !== null && typeof value === "object") {
    return checkObject(schema, object(value));
  }
  return true;
}

/**
 * Returns whether portable value satisfies a supported schema. This is not a total predicate:
 * malformed schemas and nonportable values throw ContractError rather than returning false.
 */
export function validateValue(schema: unknown, value: unknown): boolean {
  assertPortable(value);
  return checkNormalized(normalizeSchema(schema), value);
}

/** Checks portable data and schema conformance; mismatch throws ContractError with reason validation. */
export function assertValid(schema: unknown, value: unknown): asserts value is JsonValue {
  if (!validateValue(schema, value)) {
    throw new ContractError("validation", "value does not satisfy the portable schema");
  }
}

function normalizationBranch(branches: readonly JsonValue[]): number {
  if (branches.length === 2) {
    const nullIndex = branches.findIndex(
      (branch) =>
        branch !== null &&
        typeof branch === "object" &&
        !Array.isArray(branch) &&
        Object.keys(branch).length === 1 &&
        branch.type === "null",
    );
    if (nullIndex !== -1) {
      return 1 - nullIndex;
    }
  }
  return 0;
}

function signature(schema: Schema, path = "/"): JsonValue {
  if (typeof schema === "boolean") {
    return {};
  }
  const output: Record<string, JsonValue> = {};
  if (schema.default !== undefined) {
    output.default = schema.default;
  }
  if (
    schema.type === "object" ||
    schema.properties !== undefined ||
    schema.additionalProperties !== undefined
  ) {
    output.object = schema.additionalProperties === true;
    output.properties = Object.fromEntries(
      Object.entries(jsonObject(schema.properties ?? {})).map(([key, value]) => [
        key,
        signature(schemaChild(value), `${path}properties/${key}/`),
      ]),
    );
  }
  if (schema.items !== undefined) {
    const items = signature(schemaChild(schema.items), `${path}items/`);
    if (Object.keys(object(items)).length) {
      output.items = items;
    }
  }
  if (Array.isArray(schema.anyOf)) {
    const selected = normalizationBranch(schema.anyOf);
    const branches = schema.anyOf.map((branch, index) =>
      signature(schemaChild(branch), `${path}anyOf/${index}/`),
    );
    const nullable =
      schema.anyOf.length === 2 &&
      schema.anyOf.some(
        (branch) =>
          branch !== null &&
          typeof branch === "object" &&
          !Array.isArray(branch) &&
          Object.keys(branch).length === 1 &&
          branch.type === "null",
      );
    if (
      !nullable &&
      branches.some((branch) => canonicalJson(branch) !== canonicalJson(branches[selected]))
    ) {
      throw new ContractError(
        "ambiguous_normalization",
        "union branches have different cleaning/default policies",
        `${path}anyOf`,
      );
    }
    const union = object(branches[selected]);
    if (Object.keys(union).length) {
      if (Object.hasOwn(output, "object") || Object.hasOwn(output, "items")) {
        throw new ContractError(
          "ambiguous_normalization",
          "union and sibling rules both transform values",
          `${path}anyOf`,
        );
      }
      output.union = schemaChildProperty(branches, selected);
    }
  }
  return output;
}

export function assertUnambiguousInput(schema: Schema): void {
  signature(schema);
}

function transform(
  schema: Schema,
  input: JsonValue,
  defaults: boolean,
  identity: boolean,
): JsonValue {
  if (typeof schema === "boolean") {
    return clonePortable(input);
  }
  let value = clonePortable(input);
  if (Array.isArray(schema.anyOf) && schema.anyOf.length) {
    value = transform(
      schemaChild(schema.anyOf[normalizationBranch(schema.anyOf)]),
      value,
      defaults,
      identity,
    );
  }
  if (Array.isArray(value) && schema.items !== undefined) {
    value = value.map((item) => transform(schemaChild(schema.items), item, defaults, identity));
  }
  if (
    (schema.type === "object" ||
      schema.properties !== undefined ||
      schema.additionalProperties !== undefined) &&
    value !== null &&
    typeof value === "object" &&
    !Array.isArray(value)
  ) {
    value = transformObject(schema, value, defaults, identity);
  }
  return value;
}

function transformObject(
  schema: Record<string, JsonValue>,
  value: Record<string, JsonValue>,
  defaults: boolean,
  identity: boolean,
): Record<string, JsonValue> {
  const values: Record<string, JsonValue> = Object.fromEntries(Object.entries(value));
  const properties = jsonObject(schema.properties ?? {});
  for (const [key, source] of Object.entries(properties)) {
    const property = typeof source === "boolean" ? source : object(source);
    if (
      defaults &&
      !Object.hasOwn(values, key) &&
      typeof property !== "boolean" &&
      Object.hasOwn(property, "default")
    ) {
      Object.defineProperty(values, key, {
        value: clonePortable(property.default),
        enumerable: true,
        configurable: true,
        writable: true,
      });
    }
  }
  for (const key of Object.keys(values)) {
    if (!Object.hasOwn(properties, key)) {
      if (!identity && schema.additionalProperties !== true) {
        Reflect.deleteProperty(values, key);
      }
      continue;
    }
    const property = schemaChild(properties[key]);
    if (
      identity &&
      typeof property !== "boolean" &&
      Object.hasOwn(property, "default") &&
      canonicalJson(property.default) === canonicalJson(values[key])
    ) {
      Reflect.deleteProperty(values, key);
    } else {
      values[key] = transform(property, schemaChildProperty(values, key), defaults, identity);
    }
  }
  return values;
}

/**
 * Cleans input, inserts supported defaults and validates the result. Omitted or top-level null
 * input is treated as {}. Undeclared object fields are removed unless additionalProperties allows
 * them. Throws ContractError for ambiguous normalization or invalid input; does not mutate input.
 */
export function normalizeInput(schema: unknown, input: unknown = null): JsonValue {
  assertPortable(input);
  const normalized = normalizeSchema(schema);
  assertUnambiguousInput(normalized);
  const value = transform(normalized, input ?? {}, true, false);
  if (!checkNormalized(normalized, value)) {
    throw new ContractError("input", "normalized input does not satisfy schema");
  }
  return value;
}

/** Normalized input identity as canonical JSON, omitting declared properties equal to their defaults. */
export function canonicalIdentity(schema: unknown, input: unknown = null): string {
  const normalized = normalizeSchema(schema);
  return canonicalJson(transform(normalized, normalizeInput(normalized, input), false, true));
}

/**
 * Cleans portable argument fields without inserting defaults or performing final conformance
 * validation. Follow with assertValid when used outside a binding; unchanged subtrees may be shared.
 */
export function cleanArguments(schema: unknown, value: unknown): JsonValue {
  assertPortable(value);
  const normalized = normalizeSchema(schema);
  assertUnambiguousInput(normalized);
  return transform(normalized, value, false, false);
}
