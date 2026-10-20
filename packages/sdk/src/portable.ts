import { sha256 } from "@noble/hashes/sha2.js";
import { createScanner, visit } from "jsonc-parser";

import { ContractError } from "./error.js";

/** Strict portable JSON data; runtime checks additionally enforce numbers, Unicode and object shape. */
export type JsonValue =
  | null
  | boolean
  | number
  | string
  | JsonValue[]
  | { [key: string]: JsonValue };

function scalarString(value: string): void {
  for (let index = 0; index < value.length; index += 1) {
    // oxlint-disable-next-line unicorn/prefer-code-point -- scalar validation must inspect individual UTF-16 surrogate units
    const point = value.charCodeAt(index);
    if (point >= 0xd8_00 && point <= 0xdb_ff) {
      index += 1;
      // oxlint-disable-next-line unicorn/prefer-code-point -- a high surrogate must be followed by one low surrogate unit
      const next = value.charCodeAt(index);
      if (!(next >= 0xdc_00 && next <= 0xdf_ff)) {
        throw new ContractError("unicode", "unpaired high surrogate");
      }
    } else if (point >= 0xdc_00 && point <= 0xdf_ff) {
      throw new ContractError("unicode", "unpaired low surrogate");
    }
  }
}

/**
 * Validates without mutation before JSON.stringify can erase invalid data. Throws ContractError
 * for cycles, accessors, sparse arrays, non-data objects, invalid Unicode or nonportable numbers.
 * Values are limited to 128 container levels; integral numbers must be safe integers.
 */
export function assertPortable(source: unknown): asserts source is JsonValue {
  const active = new Set<object>();
  function check(value: unknown, depth: number): void {
    if (depth > 128) {
      throw new ContractError("depth", "portable value nesting limit exceeded");
    }
    if (value === null || typeof value === "boolean") {
      return;
    }
    if (typeof value === "string") {
      scalarString(value);
      return;
    }
    if (typeof value === "number") {
      if (!Number.isFinite(value) || (Number.isInteger(value) && !Number.isSafeInteger(value))) {
        throw new ContractError(
          "number",
          "number must be finite and integral values must be safe integers",
        );
      }
      return;
    }
    if (typeof value !== "object") {
      throw new ContractError("portable", "value is not strict JSON");
    }
    if (depth >= 128) {
      throw new ContractError("depth", "portable value nesting limit exceeded");
    }
    if (active.has(value)) {
      throw new ContractError("portable", "cyclic value");
    }
    const prototype: unknown = Object.getPrototypeOf(value);
    if (
      Array.isArray(value)
        ? prototype !== Array.prototype
        : prototype !== Object.prototype && prototype !== null
    ) {
      throw new ContractError("portable", "native objects are not portable");
    }
    active.add(value);
    checkContainer(value, depth);
    active.delete(value);
  }
  function checkContainer(value: object, depth: number): void {
    if (Array.isArray(value)) {
      if (Reflect.ownKeys(value).length !== value.length + 1) {
        throw new ContractError("portable", "arrays must be dense and have no custom properties");
      }
      for (let index = 0; index < value.length; index += 1) {
        const property = Object.getOwnPropertyDescriptor(value, String(index));
        if (!property || !("value" in property) || property.enumerable !== true) {
          throw new ContractError("portable", "array element is missing or an accessor");
        }
        check(property.value, depth + 1);
      }
    } else {
      for (const key of Reflect.ownKeys(value)) {
        if (typeof key !== "string") {
          throw new ContractError("portable", "symbol property");
        }
        scalarString(key);
        const property = Object.getOwnPropertyDescriptor(value, key);
        if (!property || !("value" in property) || property.enumerable !== true) {
          throw new ContractError("portable", "accessors and hidden properties are not portable");
        }
        check(property.value, depth + 1);
      }
    }
  }
  check(source, 0);
}

/** Validates and deep-copies a portable tree; invalid input throws ContractError without mutation. */
export function clonePortable(value: unknown): JsonValue {
  assertPortable(value);
  if (Array.isArray(value)) {
    return value.map(clonePortable);
  }
  if (value !== null && typeof value === "object") {
    return Object.fromEntries(
      Object.entries(value).map(([key, entry]) => [key, clonePortable(entry)]),
    );
  }
  return value;
}

function encodeCanonical(value: JsonValue): string {
  if (Array.isArray(value)) {
    return `[${value.map(encodeCanonical).join(",")}]`;
  }
  if (value !== null && typeof value === "object") {
    return `{${Object.keys(value)
      .toSorted()
      .map((key) => {
        const entry = value[key];
        if (entry === undefined) {
          throw new ContractError("portable", "missing canonical property");
        }
        return `${JSON.stringify(key)}:${encodeCanonical(entry)}`;
      })
      .join(",")}}`;
  }
  return JSON.stringify(value);
}

/** Validates and serializes canonical JSON using ECMAScript scalar spelling and UTF-16 key order. */
export function canonicalJson(value: unknown): string {
  assertPortable(value);
  return encodeCanonical(value);
}

/** Lowercase hexadecimal SHA-256 of canonicalJson(value); invalid input throws ContractError. */
export function fingerprint(value: unknown): string {
  return Array.from(sha256(new TextEncoder().encode(canonicalJson(value))), (byte) =>
    byte.toString(16).padStart(2, "0"),
  ).join("");
}

function checkNumberToken(text: string): void {
  const value = Number(text);
  if (!Number.isFinite(value)) {
    throw new ContractError("number", "non-finite parsed number");
  }
  const match = /^-?(?<integer>\d+)(?:\.(?<fractional>\d+))?(?:[eE](?<exponent>[+-]?\d+))?$/u.exec(
    text,
  );
  const groups = match?.groups;
  if (groups?.integer === undefined) {
    throw new ContractError("json", "invalid number token");
  }
  const fractional = groups.fractional ?? "";
  const digits = `${groups.integer}${fractional}`.replace(/^0+/u, "") || "0";
  const scale = Number(groups.exponent ?? "0") - fractional.length;
  const nonzero = digits !== "0";
  const integral =
    !nonzero ||
    scale >= 0 ||
    (Number.isFinite(scale) && -scale <= digits.length && digits.endsWith("0".repeat(-scale)));
  if (integral && (Math.abs(value) > Number.MAX_SAFE_INTEGER || !Number.isSafeInteger(value))) {
    throw new ContractError("number", "integral token is outside the safe range");
  }
  if (nonzero && !integral && Number.isInteger(value)) {
    throw new ContractError("number", "number loses its fractional part when parsed");
  }
}

/**
 * Parses at most 1 MiB of UTF-8 strict JSON into detached portable data. Rejects duplicate decoded
 * keys, comments, trailing commas, unsafe integral tokens and fractional precision loss.
 * Throws ContractError for invalid syntax, unsupported values or the nesting/size limits.
 */
export function parsePortableJson(text: string): JsonValue {
  if (new TextEncoder().encode(text).length > 1_048_576) {
    throw new ContractError("json_size", "JSON input exceeds compiler limit");
  }
  const scanner = createScanner(text, false);
  let depth = 0;
  // Structural delimiters are one-byte tokens; braces inside JSON strings are longer tokens.
  // Checking scanner offsets avoids importing jsonc-parser's ambient const enum at runtime.
  for (scanner.scan(); scanner.getTokenLength() !== 0; scanner.scan()) {
    if (scanner.getTokenLength() !== 1) {
      continue;
    }
    const delimiter = text[scanner.getTokenOffset()];
    if (delimiter === "{" || delimiter === "[") {
      depth += 1;
      if (depth > 128) {
        throw new ContractError("depth", "JSON nesting limit exceeded");
      }
    } else if (delimiter === "}" || delimiter === "]") {
      depth -= 1;
    }
  }
  const objects: Set<string>[] = [];
  visit(
    text,
    {
      onObjectBegin() {
        objects.push(new Set());
      },
      onObjectProperty(property) {
        const keys = objects.at(-1);
        if (keys === undefined) {
          throw new ContractError("json", "property outside object");
        }
        if (keys.has(property)) {
          throw new ContractError("duplicate", `duplicate object key ${property}`);
        }
        keys.add(property);
      },
      onObjectEnd() {
        objects.pop();
      },
      onLiteralValue(value: unknown, offset, length) {
        if (typeof value === "number") {
          checkNumberToken(text.slice(offset, offset + length));
        }
      },
      onError(code, offset) {
        throw new ContractError("json", `invalid JSON (${code}) at byte ${offset}`);
      },
    },
    {
      disallowComments: true,
      allowTrailingComma: false,
      allowEmptyContent: false,
    },
  );
  const parsed: unknown = JSON.parse(text);
  assertPortable(parsed);
  return parsed;
}
