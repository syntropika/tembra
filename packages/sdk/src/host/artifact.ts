import { sha256 } from "@noble/hashes/sha2.js";
import { init, parse } from "es-module-lexer/minimal/js";

import type { EsmArtifactV1 } from "../executor.js";
import { clonePortable } from "../portable.js";
import { executorFailure } from "./error.js";

const encoder = new TextEncoder();
const ownDescriptor = Reflect.getOwnPropertyDescriptor;
const { ownKeys } = Reflect;
const prototypeOf = Reflect.getPrototypeOf;
const sameValue = Object.is;

type Primordial = {
  readonly owner: object;
  readonly keys: readonly PropertyKey[];
  readonly descriptors: readonly (PropertyDescriptor | undefined)[];
  readonly exactKeys: boolean;
  readonly prototype: object | null;
};
const primordialRoot = globalThis;
// The pinned ASM lexer calls these constructors/statics and their prototypes. Snapshot
// complete own descriptors, including inherited typed-array helpers and symbol methods.
// This detects stability changes by trusted earlier artifacts; it is not a JS sandbox.
const primordialNames = [
  "globalThis",
  "self",
  "Object",
  "Function",
  "Array",
  "String",
  "Number",
  "Boolean",
  "RegExp",
  "Error",
  "TypeError",
  "RangeError",
  "Promise",
  "Math",
  "Reflect",
  "JSON",
  "ArrayBuffer",
  "SharedArrayBuffer",
  "DataView",
  "Int8Array",
  "Uint8Array",
  "Int16Array",
  "Uint16Array",
  "Int32Array",
  "Uint32Array",
  "Uint8ClampedArray",
  "Float32Array",
  "Float64Array",
  "BigInt64Array",
  "BigUint64Array",
  "Set",
  "Map",
  "WeakMap",
  "WeakSet",
  "Symbol",
  "TextEncoder",
  "URL",
  "Blob",
  "crypto",
] as const;
function snapshot(owner: object, keys = ownKeys(owner), exactKeys = true): Primordial {
  return {
    owner,
    keys,
    descriptors: keys.map((key) => ownDescriptor(owner, key)),
    exactKeys,
    prototype: prototypeOf(owner),
  };
}
const primordialSnapshots: Primordial[] = [snapshot(primordialRoot, [...primordialNames], false)];
const primordialOwners = new Set<object>();
function captureChain(value: unknown): void {
  if (
    (typeof value !== "object" && typeof value !== "function") ||
    value === null ||
    primordialOwners.has(value)
  ) {
    return;
  }
  primordialOwners.add(value);
  primordialSnapshots.push(snapshot(value));
  const prototype = ownDescriptor(value, "prototype");
  if (prototype !== undefined && Object.hasOwn(prototype, "value")) {
    captureChain(prototype.value);
  }
  captureChain(prototypeOf(value));
}
for (const name of primordialNames) {
  if (name === "globalThis" || name === "self") {
    continue;
  }
  const property = ownDescriptor(primordialRoot, name);
  if (property !== undefined && Object.hasOwn(property, "value")) {
    captureChain(property.value);
  }
}
// Web Crypto is exposed through native accessors in some browsers. Capture its actual
// already-created objects as well as the global descriptor, without invoking getters later.
if (typeof crypto !== "undefined") {
  captureChain(crypto);
  captureChain(crypto.subtle);
}

/** Refuses changed lexer/hash dependencies before source parsing or evaluation. */
export function assertEsmPrimordials(): void {
  for (const expected of primordialSnapshots) {
    const keys = expected.exactKeys ? ownKeys(expected.owner) : expected.keys;
    if (
      keys.length !== expected.keys.length ||
      prototypeOf(expected.owner) !== expected.prototype
    ) {
      throw executorFailure("lexer_intrinsics", "lexer dependencies changed since SDK bootstrap");
    }
    for (const [key, name] of keys.entries()) {
      const actual = ownDescriptor(expected.owner, name);
      const prior = expected.descriptors[key];
      if (
        name !== expected.keys[key] ||
        (actual === undefined) !== (prior === undefined) ||
        (actual !== undefined &&
          prior !== undefined &&
          (!sameValue(actual.value, prior.value) ||
            actual.get !== prior.get ||
            actual.set !== prior.set ||
            actual.writable !== prior.writable ||
            actual.enumerable !== prior.enumerable ||
            actual.configurable !== prior.configurable))
      ) {
        throw executorFailure("lexer_intrinsics", "lexer dependencies changed since SDK bootstrap");
      }
    }
  }
}

function scalar(value: unknown): void {
  if (typeof value !== "string") {
    throw executorFailure("artifact", "expected text");
  }
  for (let index = 0; index < value.length; index += 1) {
    // oxlint-disable-next-line unicorn/prefer-code-point -- Scalar validation must inspect individual UTF-16 code units.
    const point = value.charCodeAt(index);
    if (point >= 0xd8_00 && point <= 0xdb_ff) {
      index += 1;
      // oxlint-disable-next-line unicorn/prefer-code-point -- The second surrogate must be checked as its own UTF-16 code unit.
      const next = value.charCodeAt(index);
      if (!(next >= 0xdc_00 && next <= 0xdf_ff)) {
        throw executorFailure("json_unicode", "unpaired surrogate");
      }
    } else if (point >= 0xdc_00 && point <= 0xdf_ff) {
      throw executorFailure("json_unicode", "unpaired surrogate");
    }
  }
}

/**
 * Checks import-free ESM and the exact export set without evaluating code. Refuses static/dynamic
 * imports and import.meta, malformed Unicode and a changed checked intrinsic environment. The
 * runtime still supplies final syntax validation at evaluation; this is not a sandbox or hash check.
 */
export async function inspectEsmSource(source: string, exports: readonly string[]): Promise<void> {
  assertEsmPrimordials();
  scalar(source);
  await init();
  assertEsmPrimordials();
  let imports: ReturnType<typeof parse>[0];
  let names: string[];
  try {
    const result = parse(source);
    [imports] = result;
    names = result[1].map((entry) => entry.n);
  } catch {
    throw executorFailure("artifact_syntax", "module source could not be analyzed");
  }
  if (imports.length !== 0) {
    throw executorFailure(
      "artifact_import",
      "imports and import.meta are not in the closed single-module profile",
    );
  }
  if (names.includes("then")) {
    throw executorFailure("artifact_export", "module namespace must not export then");
  }
  if (
    new Set(names).size !== names.length ||
    names.length !== exports.length ||
    [...names].toSorted().some((name, index) => name !== [...exports].toSorted()[index])
  ) {
    throw executorFailure("artifact_export", "declared exports do not match source exports");
  }
}

/** Validates exact source bytes and closed import/export metadata without executing application code. */
export async function inspectEsmArtifact(
  artifact: EsmArtifactV1,
  selected: string,
): Promise<number> {
  assertEsmPrimordials();
  if (
    artifact === null ||
    typeof artifact !== "object" ||
    Array.isArray(artifact) ||
    Object.keys(artifact).toSorted().join(",") !== "exports,format,identity,sha256,source"
  ) {
    throw executorFailure("artifact", "invalid artifact fields");
  }
  if (artifact.format !== "tembra-esm-v1") {
    throw executorFailure("artifact", "unsupported artifact format");
  }
  scalar(artifact.identity);
  scalar(artifact.source);
  scalar(artifact.sha256);
  scalar(selected);
  if (
    !artifact.identity ||
    /\p{Cc}/u.test(artifact.identity) ||
    !/^[a-f0-9]{64}$/u.test(artifact.sha256) ||
    !Array.isArray(artifact.exports)
  ) {
    throw executorFailure("artifact", "invalid artifact metadata");
  }
  for (const name of artifact.exports) {
    scalar(name);
    if (name === "") {
      throw executorFailure("artifact_export", "export names must be nonempty");
    }
  }
  if (
    new Set(artifact.exports).size !== artifact.exports.length ||
    !artifact.exports.includes(selected)
  ) {
    throw executorFailure("artifact_export", "selected export is not declared");
  }
  const bytes = encoder.encode(artifact.source);
  if (bytes.length > 1_048_576) {
    throw executorFailure("artifact_size", "source exceeds 1 MiB");
  }
  const hash = Array.from(sha256(bytes), (byte) => byte.toString(16).padStart(2, "0")).join("");
  if (hash !== artifact.sha256) {
    throw executorFailure("artifact_hash", "source hash mismatch");
  }
  await inspectEsmSource(artifact.source, artifact.exports);
  return bytes.length;
}

function capturedArtifact(value: unknown): value is EsmArtifactV1 {
  return (
    value !== null &&
    typeof value === "object" &&
    !Array.isArray(value) &&
    "format" in value &&
    value.format === "tembra-esm-v1" &&
    "identity" in value &&
    typeof value.identity === "string" &&
    "source" in value &&
    typeof value.source === "string" &&
    "sha256" in value &&
    typeof value.sha256 === "string" &&
    "exports" in value &&
    Array.isArray(value.exports) &&
    value.exports.every((name: unknown) => typeof name === "string")
  );
}
export function captureEsmArtifact(artifact: EsmArtifactV1): EsmArtifactV1 {
  assertEsmPrimordials();
  // A private copy prevents source/metadata changes between hashing, analysis and evaluation.
  const copy: unknown = clonePortable(artifact);
  if (!capturedArtifact(copy)) {
    throw executorFailure("artifact", "invalid artifact metadata");
  }
  return copy;
}

/**
 * Copies and verifies metadata, exact UTF-8 SHA-256, source profile and selected export without
 * evaluation or generation reservation. Returns source size in bytes (maximum 1 MiB). Refusal
 * leaves admission counters unchanged and rejects with a loader error carrying code/reason.
 */
export async function validateEsmArtifact(
  artifact: EsmArtifactV1,
  selected: string,
): Promise<number> {
  return await inspectEsmArtifact(captureEsmArtifact(artifact), selected);
}
