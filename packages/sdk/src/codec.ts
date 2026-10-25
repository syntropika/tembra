import { Errors } from "./errors.ts";
import { CodecTag, CodecId } from "./protocol.ts";

import { refuse } from "./error.ts";

/** Selected once by infrastructure. Local calls and local state reads do not use a codec. */
export interface Codec {
  readonly id: string;
  serialize(value: unknown): Uint8Array;
  deserialize(bytes: Uint8Array): unknown;
}

const marker = "$tembra";

function pack(value: unknown, visiting: WeakSet<object>): unknown {
  if (typeof value === "bigint") return { [marker]: [CodecTag.Bigint, value.toString()] };
  if (value instanceof Uint8Array) return { [marker]: [CodecTag.Bytes, [...value]] };
  if (value === null || typeof value === "string" || typeof value === "boolean") return value;
  if (typeof value === "number") {
    if (!Number.isFinite(value) || (Number.isInteger(value) && !Number.isSafeInteger(value)))
      refuse(Errors.Invalid.JsonNumber, "Use BigInt for integers outside the safe Number range");
    return value;
  }
  if (typeof value !== "object")
    refuse(Errors.Invalid.JsonValue, "This JSON codec requires explicit portable values");
  if (visiting.has(value)) refuse(Errors.Invalid.JsonCycle);
  const prototype = Object.getPrototypeOf(value);
  if (!Array.isArray(value) && prototype !== Object.prototype && prototype !== null)
    refuse(Errors.Invalid.JsonType, "Supply a codec for custom object types");
  visiting.add(value);
  try {
    if (Array.isArray(value)) return Array.from(value, (child) => pack(child, visiting));
    const entries = Reflect.ownKeys(value).map((key) => {
      if (typeof key !== "string") refuse(Errors.Invalid.JsonSymbol);
      const descriptor = Object.getOwnPropertyDescriptor(value, key)!;
      if (!Object.hasOwn(descriptor, "value")) refuse(Errors.Invalid.JsonAccessor);
      return [key, pack(descriptor.value, visiting)];
    });
    return Object.hasOwn(value, marker)
      ? { [marker]: [CodecTag.Object, entries] }
      : Object.fromEntries(entries);
  } finally {
    visiting.delete(value);
  }
}

function unpack(value: unknown): unknown {
  if (value === null || typeof value !== "object") {
    if (typeof value === "number" && Number.isInteger(value) && !Number.isSafeInteger(value))
      refuse(Errors.Invalid.JsonNumber);
    return value;
  }
  if (Array.isArray(value)) return value.map(unpack);
  if (Object.hasOwn(value, marker)) {
    const tag = (value as Record<string, unknown>)[marker];
    if (Object.keys(value).length !== 1 || !Array.isArray(tag) || tag.length !== 2)
      refuse(Errors.Invalid.JsonTag);
    const [type, data] = tag;
    if (type === CodecTag.Bigint && typeof data === "string" && /^-?(0|[1-9]\d*)$/.test(data))
      return BigInt(data);
    if (
      type === CodecTag.Bytes &&
      Array.isArray(data) &&
      data.every((v) => Number.isInteger(v) && v >= 0 && v <= 255)
    )
      return Uint8Array.from(data);
    if (type === CodecTag.Object && Array.isArray(data)) {
      if (
        !data.every(
          (entry) => Array.isArray(entry) && entry.length === 2 && typeof entry[0] === "string",
        )
      )
        refuse(Errors.Invalid.JsonTag);
      return Object.fromEntries(data.map(([key, child]) => [key, unpack(child)]));
    }
    refuse(Errors.Invalid.JsonTag, "Unknown or malformed portable type tag");
  }
  return Object.fromEntries(Object.entries(value).map(([key, child]) => [key, unpack(child)]));
}

/** Also works in a minimal embedded runtime without Web APIs. */
export function utf8(text: string): Uint8Array {
  const output: number[] = [];
  for (const character of text) {
    let point = character.codePointAt(0)!;
    if (point >= 0xd800 && point <= 0xdfff) point = 0xfffd;
    if (point < 0x80) output.push(point);
    else if (point < 0x800) output.push(0xc0 | (point >> 6), 0x80 | (point & 63));
    else if (point < 0x10000)
      output.push(0xe0 | (point >> 12), 0x80 | ((point >> 6) & 63), 0x80 | (point & 63));
    else
      output.push(
        0xf0 | (point >> 18),
        0x80 | ((point >> 12) & 63),
        0x80 | ((point >> 6) & 63),
        0x80 | (point & 63),
      );
  }
  return Uint8Array.from(output);
}

export function text(bytes: Uint8Array): string {
  const output: string[] = [];
  for (let index = 0; index < bytes.length;) {
    const first = bytes[index++]!;
    if (first < 0x80) {
      output.push(String.fromCharCode(first));
      continue;
    }
    const count =
      first >= 0xc2 && first <= 0xdf
        ? 1
        : first >= 0xe0 && first <= 0xef
          ? 2
          : first >= 0xf0 && first <= 0xf4
            ? 3
            : -1;
    if (count < 0 || index + count > bytes.length) refuse(Errors.Invalid.Utf8);
    let point = first & (0x7f >> (count + 1));
    for (let offset = 0; offset < count; offset++) {
      const next = bytes[index++]!;
      if ((next & 0xc0) !== 0x80) refuse(Errors.Invalid.Utf8);
      point = (point << 6) | (next & 63);
    }
    if (
      point < [0, 0x80, 0x800, 0x10000][count]! ||
      point > 0x10ffff ||
      (point >= 0xd800 && point <= 0xdfff)
    )
      refuse(Errors.Invalid.Utf8);
    output.push(String.fromCodePoint(point));
  }
  return output.join("");
}

export const jsonCodec: Codec = Object.freeze({
  id: CodecId.Json,
  serialize: (value: unknown) => utf8(JSON.stringify(pack(value, new WeakSet()))),
  deserialize: (bytes: Uint8Array) => unpack(JSON.parse(text(bytes))),
});
